//! Linux tray: a StatusNotifierItem over D-Bus (`ksni`, pure Rust). GNOME
//! shows it with the "AppIndicator and KStatusNotifierItem Support"
//! extension (enabled by default on Zorin OS and Ubuntu); KDE, Xfce,
//! Cinnamon and others show it natively. Without a tray host the program
//! keeps running (the first-run window still opens) and the icon appears
//! as soon as a host starts.

use crate::icon::{render, Variant};
use crate::model::{view, AgentState, View};
use crate::{instance, Action};
use ksni::blocking::TrayMethods;
use ksni::menu::{MenuItem, StandardItem};
use std::sync::mpsc::Sender;

const HINT: &str = "no system tray (StatusNotifierWatcher) on the session bus. On GNOME enable the \
                    \"AppIndicator and KStatusNotifierItem Support\" extension (Extensions app, or \
                    `gnome-extensions enable ubuntu-appindicators@ubuntu.com`); FlowCursor keeps running and \
                    the icon appears once a tray is available. The status window: flow-tray --window";

struct FlowTray {
    view: View,
    actions: Sender<Action>,
}

/// ARGB32 (network byte order) icons at the sizes panels commonly use.
fn pixmaps(v: Variant) -> Vec<ksni::Icon> {
    [22, 32, 48]
        .into_iter()
        .map(|s| {
            let rgba = render(v, s);
            let mut data = Vec::with_capacity(rgba.len());
            for p in rgba.as_chunks::<4>().0 {
                data.extend_from_slice(&[p[3], p[0], p[1], p[2]]);
            }
            ksni::Icon { width: s as i32, height: s as i32, data }
        })
        .collect()
}

fn item(label: &str, enabled: bool, action: Option<Action>) -> MenuItem<FlowTray> {
    StandardItem {
        label: label.replace('_', "__"),
        enabled,
        activate: Box::new(move |t: &mut FlowTray| {
            if let Some(a) = action.clone() {
                let _ = t.actions.send(a);
            }
        }),
        ..Default::default()
    }
    .into()
}

impl ksni::Tray for FlowTray {
    fn id(&self) -> String {
        "flowcursor".into()
    }

    fn title(&self) -> String {
        "FlowCursor".into()
    }

    fn category(&self) -> ksni::Category {
        ksni::Category::Hardware
    }

    fn status(&self) -> ksni::Status {
        match self.view.variant {
            Variant::Error => ksni::Status::NeedsAttention,
            _ => ksni::Status::Active,
        }
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        pixmaps(self.view.variant)
    }

    fn attention_icon_pixmap(&self) -> Vec<ksni::Icon> {
        pixmaps(Variant::Error)
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: self.view.headline.clone(),
            description: self.view.lines.join("\n"),
            ..Default::default()
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.actions.send(Action::OpenWindow);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let v = &self.view;
        let mut m = vec![item(&v.headline, false, None)];
        for l in &v.lines {
            m.push(item(&format!("    {l}"), false, None));
        }
        m.push(MenuItem::Separator);
        m.push(item("Open status window", true, Some(Action::OpenWindow)));
        m.push(item("Open dashboard", v.dashboard_url.is_some(), Some(Action::OpenDashboard)));
        match v.paused {
            Some(true) => m.push(item("Resume sharing", true, Some(Action::SetPaused(false)))),
            Some(false) => m.push(item("Pause sharing", true, Some(Action::SetPaused(true)))),
            None => m.push(item("Pause sharing", false, None)),
        }
        if let Some(label) = &v.update_now {
            m.push(item(label, true, Some(Action::UpdateNow)));
        }
        let restart = if v.paused.is_some() { "Restart agent" } else { "Start agent" };
        m.push(item(restart, true, Some(Action::RestartAgent)));
        m.push(item("Open log", v.paused.is_some(), Some(Action::OpenLog)));
        m.push(MenuItem::Separator);
        m.push(item("Quit tray (the agent keeps running)", true, Some(Action::Quit)));
        m
    }

    fn watcher_offline(&self, reason: ksni::OfflineReason) -> bool {
        eprintln!("flow-tray: tray host went away ({reason:?}); waiting for it to come back");
        true
    }
}

/// Runs the tray until "Quit" (Ok(None)) or until an update replaced the
/// tray binary (Ok(Some(new build)): the caller starts it). `open_window`
/// opens the status window at start (`--show`).
pub fn run(open_window: bool) -> anyhow::Result<Option<String>> {
    let (tx, rx) = std::sync::mpsc::channel();
    let tray = FlowTray { view: view(&AgentState::Unknown), actions: tx.clone() };
    let handle = match tray.spawn() {
        Ok(h) => Some(h),
        Err(e) => {
            eprintln!("flow-tray: {HINT} ({e})");
            let tray = FlowTray { view: view(&AgentState::Unknown), actions: tx.clone() };
            match tray.assume_sni_available(true).spawn() {
                Ok(h) => Some(h),
                Err(e) => {
                    eprintln!("flow-tray: no D-Bus session ({e}); running without a tray icon");
                    None
                }
            }
        }
    };
    if open_window {
        instance::open_window();
    }
    let state_tx = tx.clone();
    crate::watch::spawn(crate::actions::endpoint(), move |s| {
        let _ = state_tx.send(Action::State(Box::new(s)));
    });
    let mut core = crate::TrayCore::default();
    for a in rx {
        if let Action::State(s) = &a {
            let v = view(s);
            if let Some(h) = &handle {
                h.update(|t| {
                    if t.view != v {
                        t.view = v;
                    }
                });
            }
        }
        if !core.handle(a) {
            break;
        }
    }
    if let Some(h) = handle {
        h.shutdown().wait();
    }
    Ok(core.restart_for)
}
