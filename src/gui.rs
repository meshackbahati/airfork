//! gtk window. read-only calls go straight to the library, privileged ones
//! go through pkexec on the cli binary so the ui itself stays unprivileged.

use anyhow::{Context, Result};
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{Box, Button, Label, Orientation, ScrolledWindow, TextView, WrapMode};
use libadwaita as adw;
use adw::prelude::*;

use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};
use airfork::{clients, sources, status, HotspotConfig, StatusInfo};

enum Msg {
    Refresh(StatusInfo),
    Clients(Vec<airfork::ClientInfo>),
    Started(Result<String, String>),
    Stopped(Result<(), String>),
}

type Queue = Arc<Mutex<Vec<Msg>>>;

#[derive(Clone)]
struct Ui {
    uplink: adw::ActionRow,
    channel: adw::ActionRow,
    mode: adw::ActionRow,
    session: adw::ActionRow,
    combos: adw::ExpanderRow,
    combo_rows: Arc<Mutex<Vec<adw::ActionRow>>>,
    source: adw::ComboRow,
    clients: adw::PreferencesGroup,
    client_rows: Arc<Mutex<Vec<adw::ActionRow>>>,
    start_button: Button,
    stop_button: Button,
    log: TextView,
}

fn main() {
    let app = adw::Application::builder()
        .application_id("dev.airfork.Gui")
        .build();
    app.connect_activate(build_ui);
    app.run();
}

// adw row titles go through pango markup, and capability strings contain "<="
fn escaped(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn selected_string(combo: &adw::ComboRow) -> Option<String> {
    combo
        .selected_item()
        .and_then(|item| item.downcast::<gtk4::StringObject>().ok())
        .map(|item| item.string().to_string())
}

fn log(view: &TextView, line: &str) {
    let buffer = view.buffer();
    let mut end = buffer.end_iter();
    buffer.insert(&mut end, &format!("{line}\n"));
}

fn cli_path() -> Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let sibling = exe
        .parent()
        .context("no parent directory for the gui binary")?
        .join("airfork");
    if sibling.exists() {
        return Ok(sibling);
    }
    Ok(PathBuf::from("airfork"))
}

fn privileged(args: &[String]) -> Result<String> {
    let cli = cli_path()?;
    let mut command = if airfork::is_root() {
        Command::new(&cli)
    } else {
        let mut command = Command::new("pkexec");
        command.arg(&cli);
        command
    };
    let output = command.args(args).output()?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if output.status.success() {
        Ok(stdout)
    } else {
        anyhow::bail!("{}", if stderr.is_empty() { stdout } else { stderr })
    }
}

fn update_clients(ui: &Ui, list: Vec<airfork::ClientInfo>) {
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
        let title = client.hostname.as_deref().unwrap_or(&client.mac).to_string();
        let subtitle = format!(
            "{} · {}",
            client.ip.as_deref().unwrap_or("no address"),
            if client.connected { "connected" } else { "leased" }
        );
        let row = adw::ActionRow::builder()
            .title(escaped(&title))
            .subtitle(escaped(&subtitle))
            .build();
        ui.clients.add(&row);
        rows.push(row);
    }
}

fn handle_msg(msg: Msg, ui: &Ui) {
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
                    if cap.concurrent { "supported" } else { "unsupported" },
                    if cap.same_channel_required { "required" } else { "optional" }
                )));

                {
                    let mut added = ui.combo_rows.lock().unwrap();
                    for row in added.iter() {
                        ui.combos.remove(row);
                    }
                    added.clear();
                    for combination in &cap.combinations {
                        let row = adw::ActionRow::builder().title(escaped(combination)).build();
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

fn build_ui(app: &adw::Application) {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("airfork")
        .default_width(940)
        .default_height(660)
        .build();

    let header = adw::HeaderBar::new();
    let refresh = Button::from_icon_name("view-refresh-symbolic");
    refresh.set_tooltip_text(Some("Refresh adapter state"));
    header.pack_end(&refresh);

    let page = adw::PreferencesPage::new();

    let adapter = adw::PreferencesGroup::builder()
        .title("Adapter")
        .description("what this machine can do")
        .build();
    let uplink = adw::ActionRow::builder()
        .title("Uplink")
        .subtitle("checking...")
        .build();
    let channel = adw::ActionRow::builder().title("Channel").subtitle("—").build();
    let mode = adw::ActionRow::builder()
        .title("Concurrent AP + station")
        .subtitle("—")
        .build();
    let session = adw::ActionRow::builder()
        .title("Session")
        .subtitle("not running")
        .build();
    let combos = adw::ExpanderRow::builder()
        .title("Interface combinations")
        .subtitle("driver details")
        .build();
    adapter.add(&uplink);
    adapter.add(&channel);
    adapter.add(&mode);
    adapter.add(&session);
    adapter.add(&combos);
    page.add(&adapter);

    let hotspot = adw::PreferencesGroup::builder()
        .title("Hotspot")
        .description("what connecting devices will see")
        .build();
    let ssid = adw::EntryRow::builder()
        .title("Network name")
        .text("LabHotspot")
        .build();
    let password = adw::PasswordEntryRow::builder()
        .title("Password")
        .text("airfork-2026")
        .build();
    let iface = adw::EntryRow::builder()
        .title("Virtual interface")
        .text("wlan0_ap")
        .build();
    hotspot.add(&ssid);
    hotspot.add(&password);
    hotspot.add(&iface);
    page.add(&hotspot);

    let source_group = adw::PreferencesGroup::builder()
        .title("Internet source")
        .description("which interface carries the hotspot traffic")
        .build();
    let source = adw::ComboRow::builder().title("Share connection from").build();
    source_group.add(&source);
    page.add(&source_group);

    let client_group = adw::PreferencesGroup::builder()
        .title("Clients")
        .description("devices joined to the hotspot")
        .build();

    // two columns so the window uses the desktop when it is opened wide
    let root = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(16)
        .margin_start(16)
        .margin_end(16)
        .margin_top(16)
        .margin_bottom(16)
        .build();
    root.set_hexpand(true);
    root.set_vexpand(true);

    let left = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(12)
        .build();
    left.set_hexpand(true);
    left.set_vexpand(true);
    let left_clamp = adw::Clamp::new();
    left_clamp.set_maximum_size(460);
    left_clamp.set_child(Some(&page));
    left_clamp.set_hexpand(true);
    left_clamp.set_vexpand(true);
    left.append(&left_clamp);

    let actions = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(8)
        .halign(gtk4::Align::Start)
        .build();
    let start = Button::with_label("Start hotspot");
    start.add_css_class("suggested-action");
    let stop = Button::with_label("Stop hotspot");
    stop.set_sensitive(false);
    actions.append(&start);
    actions.append(&stop);
    left.append(&actions);

    let right = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(12)
        .build();
    right.set_hexpand(true);
    right.set_vexpand(true);
    let client_page = adw::PreferencesPage::new();
    client_page.add(&client_group);
    let right_clamp = adw::Clamp::new();
    right_clamp.set_maximum_size(460);
    right_clamp.set_child(Some(&client_page));
    right_clamp.set_hexpand(true);
    right.append(&right_clamp);

    let output = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(6)
        .build();
    output.set_vexpand(true);
    let output_label = Label::builder().label("Output").xalign(0.0).build();
    output_label.add_css_class("dim-label");
    let log_view = TextView::builder()
        .editable(false)
        .wrap_mode(WrapMode::Word)
        .build();
    log_view.add_css_class("monospace");
    log_view.set_height_request(160);
    log_view.set_vexpand(true);
    let scroller = ScrolledWindow::builder()
        .child(&log_view)
        .min_content_height(160)
        .build();
    scroller.set_vexpand(true);
    output.append(&output_label);
    output.append(&scroller);
    right.append(&output);

    root.append(&left);
    root.append(&right);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&root));
    window.set_content(Some(&toolbar));
    window.present();

    let ui = Ui {
        uplink: uplink.clone(),
        channel: channel.clone(),
        mode: mode.clone(),
        session: session.clone(),
        combos: combos.clone(),
        combo_rows: Arc::new(Mutex::new(Vec::new())),
        source: source.clone(),
        clients: client_group.clone(),
        client_rows: Arc::new(Mutex::new(Vec::new())),
        start_button: start.clone(),
        stop_button: stop.clone(),
        log: log_view.clone(),
    };

    let queue: Queue = Arc::new(Mutex::new(Vec::new()));
    let ui_queue = queue.clone();
    let ui_state = ui.clone();
    glib::source::idle_add_local(move || {
        let mut items = ui_queue.lock().unwrap();
        while let Some(msg) = items.pop() {
            handle_msg(msg, &ui_state);
        }
        drop(items);
        glib::ControlFlow::Continue
    });

    let timer_queue = queue.clone();
    glib::source::timeout_add_local(std::time::Duration::from_secs(2), move || {
        let mut queue = timer_queue.lock().unwrap();
        queue.push(Msg::Clients(airfork::clients()));
        queue.push(Msg::Refresh(airfork::status()));
        glib::ControlFlow::Continue
    });

    let refresh_queue = queue.clone();
    refresh.connect_clicked(move |_| {
        refresh_queue.lock().unwrap().push(Msg::Refresh(status()));
    });

    let start_queue = queue.clone();
    let ssid_value = ssid.text().to_string();
    let pass_value = password.text().to_string();
    let iface_value = iface.text().to_string();
    start.connect_clicked(move |_| {
        let config = HotspotConfig {
            ssid: ssid_value.clone(),
            password: pass_value.clone(),
            ap_interface: iface_value.clone(),
            country: "KE".into(),
            uplink: selected_string(&ui.source),
            ap_base: None,
            channel: None,
            hidden: false,
            dry_run: false,
        };
        if let Err(error) = config.validate() {
            log(&ui.log, &format!("bad config: {error}"));
            return;
        }
        let queue = start_queue.clone();
        std::thread::spawn(move || {
            let args = vec![
                "start".to_string(),
                "--ssid".to_string(),
                config.ssid.clone(),
                "--password".to_string(),
                config.password.clone(),
                "--ap-interface".to_string(),
                config.ap_interface.clone(),
            ];
            let mut args = args;
            if let Some(source) = &config.uplink {
                args.push("--uplink".to_string());
                args.push(source.clone());
            }
            let result = privileged(&args).map_err(|error| error.to_string());
            queue.lock().unwrap().push(Msg::Started(result));
            queue.lock().unwrap().push(Msg::Refresh(status()));
        });
    });

    let stop_queue = queue.clone();
    stop.connect_clicked(move |_| {
        let queue = stop_queue.clone();
        std::thread::spawn(move || {
            let result = privileged(&["stop".to_string()])
                .map(|_| ())
                .map_err(|error| error.to_string());
            queue.lock().unwrap().push(Msg::Stopped(result));
            queue.lock().unwrap().push(Msg::Refresh(status()));
        });
    });

    let initial_queue = queue.clone();
    std::thread::spawn(move || {
        initial_queue.lock().unwrap().push(Msg::Refresh(status()));
    });
}
