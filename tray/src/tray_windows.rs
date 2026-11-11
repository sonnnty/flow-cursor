//! Windows notification-area icon (implemented in a later step).

pub fn run(_show: bool, _start_agent: bool) -> anyhow::Result<()> {
    anyhow::bail!("the Windows tray is not built yet")
}

pub fn quit_running() -> bool {
    false
}
