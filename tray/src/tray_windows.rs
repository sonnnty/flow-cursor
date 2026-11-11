//! Windows notification-area icon (`tray-icon`, plain Win32 underneath).
//!
//! The tray is what starts at login (`HKCU\...\Run`); it starts the agent
//! detached from any console when nothing answers on the agent's pipe
//! (unless `--no-start-agent`, used when the agent runs elevated from a
//! scheduled task). "Quit" stops the agent too: on Windows the tray is the
//! visible face of FlowCursor, so quitting it should not leave an
//! invisible agent behind. `flow-tray --quit` (installer, upgrades) only
//! closes the tray.

use crate::icon::{render, Variant};
use crate::model::{view, AgentState, View};
use crate::{instance, win, Action, TrayCore};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

/// Class of the hidden window (`FindWindow` in `--quit` and the installer).
pub const WINDOW_CLASS: &str = "FlowCursorTray";

fn icon(v: Variant) -> Icon {
    let s = win::small_icon_size();
    Icon::from_rgba(render(v, s), s, s).expect("valid icon size")
}

/// The notification-area tooltip is limited to 127 characters.
fn tooltip(v: &View) -> String {
    let mut t = v.headline.clone();
    for l in v.lines.iter().take(3) {
        t.push('\n');
        t.push_str(l);
    }
    match t.char_indices().nth(127) {
        Some((i, _)) => t[..i].to_string(),
        None => t,
    }
}

fn action_for(id: &str) -> Option<Action> {
    Some(match id {
        "open" => Action::OpenWindow,
        "dashboard" => Action::OpenDashboard,
        "pause" => Action::SetPaused(true),
        "resume" => Action::SetPaused(false),
        "restart" => Action::RestartAgent,
        "update" => Action::UpdateNow,
        "log" => Action::OpenLog,
        "quit" => Action::Quit,
        _ => return None,
    })
}

fn menu(v: &View) -> Menu {
    let m = Menu::new();
    let add = |id: &str, text: &str, enabled: bool| {
        // `&` starts an accelerator in Win32 menus.
        let _ = m.append(&MenuItem::with_id(id, text.replace('&', "&&"), enabled, None));
    };
    add("status-head", &v.headline, false);
    for (i, l) in v.lines.iter().enumerate() {
        add(&format!("status-{i}"), &format!("    {l}"), false);
    }
    let _ = m.append(&PredefinedMenuItem::separator());
    add("open", "Open status window", true);
    add("dashboard", "Open dashboard", v.dashboard_url.is_some());
    match v.paused {
        Some(true) => add("resume", "Resume sharing", true),
        Some(false) => add("pause", "Pause sharing", true),
        None => add("pause", "Pause sharing", false),
    }
    if let Some(label) = &v.update_now {
        add("update", label, true);
    }
    add("restart", if v.paused.is_some() { "Restart agent" } else { "Start agent" }, true);
    add("log", "Open log", v.paused.is_some());
    let _ = m.append(&PredefinedMenuItem::separator());
    add("quit", "Quit FlowCursor", true);
    m
}

/// Runs the tray until "Quit" or WM_CLOSE (Ok(None)), or until an update
/// replaced the tray binary (Ok(Some(new build)): the caller starts it).
pub fn run(show: bool, start_agent: bool) -> anyhow::Result<Option<String>> {
    let hwnd = win::create_hidden_window(WINDOW_CLASS)?;
    let wake_hwnd = hwnd as isize;
    let (tx, rx) = std::sync::mpsc::channel::<Action>();
    {
        let tx = tx.clone();
        MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
            if let Some(a) = action_for(&e.id.0) {
                let _ = tx.send(a);
                win::wake(wake_hwnd);
            }
        }));
    }
    {
        let tx = tx.clone();
        TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
            let open = matches!(
                e,
                TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. }
                    | TrayIconEvent::DoubleClick { button: MouseButton::Left, .. }
            );
            if open {
                let _ = tx.send(Action::OpenWindow);
                win::wake(wake_hwnd);
            }
        }));
    }

    let mut current = view(&AgentState::Unknown);
    let tray = TrayIconBuilder::new()
        .with_id("flowcursor")
        .with_icon(icon(current.variant))
        .with_tooltip(tooltip(&current))
        .with_menu(Box::new(menu(&current)))
        .with_menu_on_left_click(false)
        .build()?;
    if show {
        instance::open_window();
    }
    crate::watch::spawn(crate::actions::endpoint(), move |s| {
        let _ = tx.send(Action::State(Box::new(s)));
        win::wake(wake_hwnd);
    });

    let mut core = TrayCore { start_agent, quit_stops_agent: true, ..Default::default() };
    while win::pump_one() {
        while let Ok(a) = rx.try_recv() {
            if let Action::State(s) = &a {
                let v = view(s);
                if v != current {
                    if v.variant != current.variant {
                        let _ = tray.set_icon(Some(icon(v.variant)));
                    }
                    let _ = tray.set_tooltip(Some(tooltip(&v)));
                    tray.set_menu(Some(Box::new(menu(&v))));
                    current = v;
                }
            }
            if !core.handle(a) {
                return Ok(core.restart_for.take());
            }
        }
    }
    // WM_CLOSE (flow-tray --quit, installer): only the tray exits.
    Ok(None)
}

/// Closes the running tray (`flow-tray --quit`); false when none runs.
pub fn quit_running() -> bool {
    win::close_window(WINDOW_CLASS)
}
