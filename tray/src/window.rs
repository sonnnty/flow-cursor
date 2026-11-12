//! Status window: everything the agent reports, the recent log, buttons
//! for the common actions and the join form (first run / re-join).
//!
//! egui (eframe, glow backend). Runs in its own short-lived process
//! (`flow-tray --window`) so the tray itself never keeps an OpenGL context
//! or a window event loop around.

use crate::icon::{self, Variant};
use crate::model::{self, AgentState};
use crate::{actions, instance};
use eframe::egui::{self, Color32, RichText};
use flow_ipc::{Command, Reply, Snapshot};
use std::collections::HashMap;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Saves a screenshot of the window to this PNG path and exits (docs,
/// headless tests).
const ENV_SCREENSHOT: &str = "FLOW_TRAY_SCREENSHOT";
const LOG_LINES: usize = 300;
const LOG_REFRESH: Duration = Duration::from_secs(2);

const RED: Color32 = Color32::from_rgb(0xd6, 0x33, 0x2f);
const AMBER: Color32 = Color32::from_rgb(0xc2, 0x7c, 0x0e);
const GREEN: Color32 = Color32::from_rgb(0x22, 0x8b, 0x3d);

/// Result of a button click, shown under the buttons.
type Outcome = (bool, String);

struct StatusApp {
    state: Arc<Mutex<AgentState>>,
    logs: Arc<Mutex<Vec<String>>>,
    logs_fetched: Option<Instant>,
    token: String,
    hub: String,
    hub_prefilled: bool,
    outcome: Option<Outcome>,
    outcome_tx: Sender<Outcome>,
    outcome_rx: Receiver<Outcome>,
    icons: HashMap<Variant, egui::TextureHandle>,
    screenshot: Option<(std::path::PathBuf, Instant, bool)>,
}

pub fn run() -> anyhow::Result<()> {
    let state = Arc::new(Mutex::new(AgentState::Unknown));
    let (outcome_tx, outcome_rx) = channel();
    let app_icon = egui::IconData { rgba: icon::render(Variant::App, 64), width: 64, height: 64 };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("FlowCursor")
            // Matches flowcursor.desktop (StartupWMClass): GNOME shows the
            // app icon and name for this window.
            .with_app_id("flowcursor")
            .with_icon(app_icon)
            .with_inner_size([600.0, 760.0])
            .with_min_inner_size([420.0, 360.0]),
        ..Default::default()
    };
    let screenshot = std::env::var_os(ENV_SCREENSHOT).map(|p| (std::path::PathBuf::from(p), Instant::now(), false));
    eframe::run_native(
        "FlowCursor",
        options,
        Box::new(move |cc| {
            install_fallback_fonts(&cc.egui_ctx);
            let ctx = cc.egui_ctx.clone();
            let st = state.clone();
            crate::watch::spawn(actions::endpoint(), move |s| {
                *st.lock().expect("poisoned") = s;
                ctx.request_repaint();
            });
            Ok(Box::new(StatusApp {
                state,
                logs: Arc::default(),
                logs_fetched: None,
                token: String::new(),
                hub: String::new(),
                hub_prefilled: false,
                outcome: None,
                outcome_tx,
                outcome_rx,
                icons: HashMap::new(),
                screenshot,
            }))
        }),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}

/// egui's built-in fonts have no Thai glyphs (peer names, hostnames): add
/// a system font with Thai as a fallback when one is installed.
fn install_fallback_fonts(ctx: &egui::Context) {
    const CANDIDATES: &[&str] = &[
        // Windows 10/11
        r"C:\Windows\Fonts\LeelawUI.ttf",
        r"C:\Windows\Fonts\leelawad.ttf",
        // Ubuntu / Zorin (fonts-thai-tlwg, fonts-noto)
        "/usr/share/fonts/truetype/noto/NotoSansThai-Regular.ttf",
        "/usr/share/fonts/opentype/noto/NotoSansThai-Regular.ttf",
        "/usr/share/fonts/truetype/tlwg/Garuda.ttf",
        "/usr/share/fonts/opentype/tlwg/Garuda.otf",
        "/usr/share/fonts/truetype/tlwg/Loma.ttf",
    ];
    let Some(data) = CANDIDATES.iter().find_map(|p| std::fs::read(p).ok()) else {
        return;
    };
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert("thai".into(), Arc::new(egui::FontData::from_owned(data)));
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts.families.entry(family).or_default().push("thai".into());
    }
    ctx.set_fonts(fonts);
}

impl StatusApp {
    fn icon(&mut self, ctx: &egui::Context, v: Variant) -> egui::TextureHandle {
        self.icons
            .entry(v)
            .or_insert_with(|| {
                let img = egui::ColorImage::from_rgba_unmultiplied([48, 48], &icon::render(v, 48));
                ctx.load_texture(format!("state-{}", v.name()), img, egui::TextureOptions::LINEAR)
            })
            .clone()
    }

    /// Runs `f` on a thread; its result is shown under the buttons.
    fn spawn(&self, ctx: &egui::Context, f: impl FnOnce() -> std::io::Result<String> + Send + 'static) {
        let tx = self.outcome_tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let r = match f() {
                Ok(m) => (true, m),
                Err(e) => (false, e.to_string()),
            };
            let _ = tx.send(r);
            ctx.request_repaint();
        });
    }

    fn refresh_logs(&mut self, ctx: &egui::Context, reachable: bool) {
        if !reachable || self.logs_fetched.is_some_and(|t| t.elapsed() < LOG_REFRESH) {
            return;
        }
        self.logs_fetched = Some(Instant::now());
        let logs = self.logs.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            if let Ok(Reply::Logs { lines }) =
                flow_ipc::client::request(&actions::endpoint(), &Command::Logs { max: LOG_LINES })
            {
                *logs.lock().expect("poisoned") = lines;
                ctx.request_repaint();
            }
        });
    }

    fn buttons(&mut self, ui: &mut egui::Ui, snap: Option<&Snapshot>) {
        let ctx = ui.ctx().clone();
        ui.horizontal_wrapped(|ui| {
            match snap.map(|s| s.paused) {
                Some(true) => {
                    if ui.button("Resume sharing").clicked() {
                        self.spawn(&ctx, || actions::set_paused(false));
                    }
                }
                Some(false) => {
                    if ui
                        .button("Pause sharing")
                        .on_hover_text("Stop crossing to other machines and refuse being controlled, like away")
                        .clicked()
                    {
                        self.spawn(&ctx, || actions::set_paused(true));
                    }
                }
                None => {
                    ui.add_enabled(false, egui::Button::new("Pause sharing"));
                }
            }
            if let Some(v) = snap.filter(|s| s.update.can_update_now).and_then(|s| s.update.available.clone()) {
                if ui
                    .button(format!("Update now to {v}"))
                    .on_hover_text("Installs the hub's version as soon as no machine is being controlled")
                    .clicked()
                {
                    self.spawn(&ctx, actions::update_now);
                }
            }
            let restart = if snap.is_some() { "Restart agent" } else { "Start agent" };
            if ui.button(restart).clicked() {
                self.spawn(&ctx, actions::restart_agent);
            }
            let url = snap.and_then(|s| s.dashboard_url.clone());
            if ui.add_enabled(url.is_some(), egui::Button::new("Open dashboard")).clicked() {
                if let Some(u) = url {
                    self.spawn(&ctx, move || actions::open(&u).map(|_| format!("opened {u}")));
                }
            }
            let log_file = snap.and_then(|s| s.log_file.clone());
            if ui.add_enabled(snap.is_some(), egui::Button::new("Open log")).clicked() {
                self.spawn(&ctx, move || actions::open_log(log_file.as_deref()).map(|_| "log opened".into()));
            }
        });
        if let Some((ok, msg)) = &self.outcome {
            let color = if *ok { GREEN } else { RED };
            ui.colored_label(color, msg);
        }
    }

    fn join_form(&mut self, ui: &mut egui::Ui, snap: Option<&Snapshot>) {
        let needed = snap.is_some_and(model::needs_join);
        let title = if needed { "Join a hub" } else { "Join a hub (re-join / another hub)" };
        egui::CollapsingHeader::new(RichText::new(title).strong()).default_open(needed).show(ui, |ui| {
            if needed {
                ui.label(
                    "This machine is not joined to a FlowCursor hub yet. Copy the join token from the hub \
                     dashboard (Overview, Join token) and paste it here.",
                );
            }
            if !self.hub_prefilled {
                if let Some(h) = snap.and_then(|s| s.hub_address.clone()) {
                    self.hub = h;
                }
                self.hub_prefilled = snap.is_some();
            }
            egui::Grid::new("join").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                ui.label("Join token");
                ui.add(egui::TextEdit::singleline(&mut self.token).hint_text("FC1-…").desired_width(360.0));
                ui.end_row();
                ui.label("Hub address");
                ui.add(
                    egui::TextEdit::singleline(&mut self.hub)
                        .hint_text("e.g. 192.168.1.10 (empty: find the hub with mDNS)")
                        .desired_width(360.0),
                );
                ui.end_row();
            });
            let can = snap.is_some() && self.token.trim().starts_with("FC1-");
            if ui.add_enabled(can, egui::Button::new("Join")).clicked() {
                let (t, h) = (self.token.clone(), self.hub.clone());
                self.spawn(ui.ctx(), move || actions::join(&t, &h));
                self.token.clear();
            }
            if snap.is_none() {
                ui.label("(the agent must be running to join)");
            }
        });
    }
}

fn row(ui: &mut egui::Ui, k: &str, v: impl Into<egui::WidgetText>) {
    ui.label(RichText::new(k).weak());
    ui.add(egui::Label::new(v).wrap());
    ui.end_row();
}

fn grid(ui: &mut egui::Ui, id: &str, f: impl FnOnce(&mut egui::Ui)) {
    let value_width = (ui.available_width() - 150.0).max(200.0);
    egui::Grid::new(id).num_columns(2).spacing([16.0, 4.0]).max_col_width(value_width).striped(true).show(ui, f);
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

fn yes_no(b: bool) -> &'static str {
    if b {
        "yes"
    } else {
        "no"
    }
}

fn details(ui: &mut egui::Ui, s: &Snapshot) {
    for p in &s.problems {
        ui.colored_label(RED, format!("Problem: {p}"));
    }
    if let Some(r) = &s.rejected {
        ui.colored_label(RED, format!("The hub refused this machine: {r}"));
    }
    for w in &s.warnings {
        ui.colored_label(AMBER, w);
    }
    // Closed on purpose (away, paused, not connected): an explanation, not
    // a problem.
    if let Some(n) = model::idle_note(s) {
        ui.label(RichText::new(n).weak());
    }

    ui.add_space(6.0);
    ui.heading("Hub");
    grid(ui, "hub", |ui| {
        row(ui, "Address", model::hub_label(s));
        let conn = if s.hub_connected {
            RichText::new("connected").color(GREEN)
        } else {
            let why = s.hub_error.as_deref().map(|e| format!(" — {e}")).unwrap_or_default();
            RichText::new(format!("not connected{why}")).color(RED)
        };
        row(ui, "Connection", conn);
        row(ui, "Fingerprint", RichText::new(s.hub_fingerprint.as_deref().unwrap_or("(not pinned yet)")).monospace());
        ui.label(RichText::new("Dashboard").weak());
        match &s.dashboard_url {
            Some(u) => ui.hyperlink(u),
            None => ui.label("(hub address unknown)"),
        };
        ui.end_row();
    });

    ui.add_space(6.0);
    ui.heading("Machines");
    if s.peers.is_empty() {
        ui.label("No other machines at the hub yet.");
    } else {
        egui::Grid::new("peers").num_columns(4).spacing([16.0, 4.0]).striped(true).show(ui, |ui| {
            for h in ["Name", "State", "Path", "RTT"] {
                ui.label(RichText::new(h).strong());
            }
            ui.end_row();
            for p in &s.peers {
                ui.label(if p.name.is_empty() { format!("node {}", p.id) } else { p.name.clone() });
                let (state, color) = match (p.enabled, p.online, p.away) {
                    (false, _, _) => ("disabled", Color32::GRAY),
                    (_, false, _) => ("offline", Color32::GRAY),
                    (_, true, true) => ("away", AMBER),
                    _ => ("online", GREEN),
                };
                let focus = if s.controlling.as_ref().is_some_and(|c| c.id == p.id) {
                    " · you control it"
                } else if s.controlled_by.iter().any(|c| c.id == p.id) {
                    " · controls you"
                } else {
                    ""
                };
                ui.colored_label(color, format!("{state}{focus}"));
                ui.label(p.path.as_deref().unwrap_or("-"));
                ui.label(p.rtt_ms.map(|r| format!("{r:.1} ms")).unwrap_or_else(|| "-".into()));
                ui.end_row();
            }
        });
    }
    ui.add_space(6.0);
    ui.heading("This machine");
    grid(ui, "node", |ui| {
        row(ui, "Name", model::node_label(s));
        row(ui, "Node id", s.node_id.map(|n| n.to_string()).unwrap_or_else(|| "-".into()));
        let sharing = match (s.paused, s.away) {
            (true, _) => RichText::new("paused (Resume sharing to turn it back on)").color(AMBER),
            (false, true) => RichText::new("away: not on an allowed network").color(AMBER),
            _ => RichText::new("on").color(GREEN),
        };
        row(ui, "Sharing", sharing);
        row(ui, "Focus", model::focus_label(s));
        row(ui, "Crossing key", if s.crossing_key.is_empty() { "none (cross freely)" } else { &s.crossing_key });
        row(ui, "Emergency hotkey", &s.emergency_hotkey);
        row(
            ui,
            "Input",
            format!(
                "capture {} · inject {} · clipboard {}",
                yes_no(s.can_capture),
                model::inject_label(s),
                yes_no(s.can_clipboard)
            ),
        );
        if let Some(c) = model::capture_label(s) {
            row(ui, "Edge capture", c);
        }
        row(ui, "Version", model::version_label(s));
        row(ui, "Agent", format!("pid {}, backend {}", s.pid, s.backend));
        let policy = if s.update.policy.is_empty() { "(not known yet)" } else { s.update.policy.as_str() };
        row(ui, "Update policy", policy);
        let update = match model::update_label(s) {
            Some(u) if s.update.state == "unsupported" || s.update.failed.is_some() => RichText::new(u).color(AMBER),
            Some(u) => RichText::new(u),
            None => RichText::new("up to date with the hub").color(GREEN),
        };
        row(ui, "Update", update);
        if let Some(r) = &s.update.update_only {
            row(ui, "Hub", RichText::new(format!("connected only to update: {r}")).color(RED));
        }
        // (When current, the Update row already shows the failure.)
        if s.update.state != "current" {
            if let Some(f) = &s.update.failed {
                let rb = if s.update.failed_rolled_back { " (previous version restored)" } else { "" };
                row(ui, "Last failure", RichText::new(format!("{f}{rb}")).color(AMBER));
            }
        }
        row(ui, "Status window", flow_version::FULL);
        row(ui, "Config file", RichText::new(&s.config_file).monospace());
        row(
            ui,
            "Log",
            RichText::new(s.log_file.as_deref().unwrap_or("systemd journal (journalctl --user -u flow-agent)"))
                .monospace(),
        );
    });

    ui.add_space(6.0);
    ui.heading("Network");
    grid(ui, "net", |ui| {
        let allowed = if s.allowed_networks.is_empty() {
            "any (no restriction)".to_string()
        } else {
            s.allowed_networks.join(", ")
        };
        row(ui, "Allowed networks", allowed);
        let status = if s.away {
            RichText::new("not on an allowed network (away)").color(AMBER)
        } else {
            RichText::new("allowed").color(GREEN)
        };
        row(ui, "This network", status);
        row(ui, "Interface addresses", if s.local_addrs.is_empty() { "-".into() } else { s.local_addrs.join("\n") });
    });
}

impl eframe::App for StatusApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = &ui.ctx().clone();
        while let Ok(o) = self.outcome_rx.try_recv() {
            self.outcome = Some(o);
        }
        if instance::take_focus_request() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
        let state = self.state.lock().expect("poisoned").clone();
        let snap = state.snapshot().cloned();
        self.refresh_logs(ctx, snap.is_some());
        let view = model::view(&state);

        egui::Panel::top("head").show(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let tex = self.icon(ctx, view.variant);
                ui.image((tex.id(), egui::vec2(40.0, 40.0)));
                ui.vertical(|ui| {
                    ui.heading(capitalize(view.headline.trim_start_matches("FlowCursor: ")));
                    let sub = match &state {
                        AgentState::Unknown => "Connecting to the local agent…".to_string(),
                        AgentState::Unreachable(e) => {
                            format!("Nothing answers on {} ({e})", actions::endpoint().display())
                        }
                        AgentState::Running(s) => format!("{} · hub {}", model::node_label(s), model::hub_label(s)),
                    };
                    ui.label(RichText::new(sub).weak());
                });
            });
            ui.add_space(4.0);
            self.buttons(ui, snap.as_ref());
            ui.add_space(4.0);
        });

        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                if let AgentState::Unreachable(_) = state {
                    ui.colored_label(
                        RED,
                        "The FlowCursor agent is not running. Start it with the button above (Linux: \
                         systemctl --user start flow-agent).",
                    );
                    ui.add_space(6.0);
                }
                self.join_form(ui, snap.as_ref());
                if let Some(s) = &snap {
                    details(ui, s);
                }
                ui.add_space(6.0);
                egui::CollapsingHeader::new(RichText::new("Recent log").strong()).default_open(true).show(ui, |ui| {
                    let lines = self.logs.lock().expect("poisoned").clone();
                    egui::ScrollArea::vertical().id_salt("log").max_height(220.0).stick_to_bottom(true).show(
                        ui,
                        |ui| {
                            for l in lines.iter() {
                                ui.label(RichText::new(l).monospace().size(11.0));
                            }
                        },
                    );
                });
            });
        });

        self.take_screenshot(ctx);
        ctx.request_repaint_after(Duration::from_millis(500));
    }
}

impl StatusApp {
    /// With `FLOW_TRAY_SCREENSHOT=<png>`: after the window settled, save it
    /// and close.
    fn take_screenshot(&mut self, ctx: &egui::Context) {
        let Some((path, started, requested)) = &mut self.screenshot else {
            return;
        };
        if *requested || started.elapsed() < Duration::from_secs(4) {
            return;
        }
        *requested = true;
        let (path, ctx2) = (path.clone(), ctx.clone());
        ctx.request_screenshot(move |img| {
            let [w, h] = img.size;
            let rgba: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_array()).collect();
            let r = std::fs::File::create(&path).map_err(|e| e.to_string()).and_then(|f| {
                let mut e = png::Encoder::new(std::io::BufWriter::new(f), w as u32, h as u32);
                e.set_color(png::ColorType::Rgba);
                e.set_depth(png::BitDepth::Eight);
                e.write_header().and_then(|mut w| w.write_image_data(&rgba)).map_err(|e| e.to_string())
            });
            match r {
                Ok(()) => eprintln!("flow-tray: screenshot saved to {}", path.display()),
                Err(e) => eprintln!("flow-tray: screenshot: {e}"),
            }
            ctx2.send_viewport_cmd(egui::ViewportCommand::Close);
            ctx2.request_repaint();
        });
    }
}
