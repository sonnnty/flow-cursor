//! The tray restarts itself after an update: a new, detached process
//! (new pid, so a new D-Bus name) takes over from the old one, which exits;
//! the single-instance lock is handed over in the right order and the new
//! instance is not refused by the old one.
//!
//! Runs the real `flow-tray` against a stand-in agent on a private IPC
//! socket, without a D-Bus session (the tray runs without an icon then; the
//! lock and the restart do not depend on it).
#![cfg(unix)]

use flow_ipc::{Command, Reply, Snapshot, UpdateInfo};
use std::io::Write;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const T: Duration = Duration::from_secs(15);
/// What the stand-in agent says the updated tray build is.
const NEW_BUILD: &str = "9.9.9+restart-test";

struct Env {
    dir: PathBuf,
}

impl Env {
    fn new(tag: &str) -> Env {
        let dir = std::env::temp_dir().join(format!("flow-tray-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Env { dir }
    }

    fn tray(&self, args: &[&str]) -> std::process::Command {
        let mut c = std::process::Command::new(env!("CARGO_BIN_EXE_flow-tray"));
        c.args(args)
            .env("XDG_RUNTIME_DIR", &self.dir)
            .env("FLOW_AGENT_IPC", self.dir.join("agent.sock"))
            .env("HOME", &self.dir)
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .env_remove("FLOW_TRAY_REEXEC")
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .stdin(Stdio::null())
            .stdout(Stdio::null());
        c
    }

    /// Pid the tray wrote into its lock file.
    fn lock_pid(&self) -> Option<u32> {
        std::fs::read_to_string(self.dir.join("flowcursor/tray.lock")).ok()?.trim().parse().ok()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        // Whatever tray is left.
        let _ = self.tray(&["--quit"]).stderr(Stdio::null()).status();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A stand-in agent: answers `Subscribe` with a joined machine's status; the
/// installed tray build is announced once `updated` is set.
fn fake_agent(sock: &Path, updated: Arc<AtomicBool>) {
    let l = UnixListener::bind(sock).unwrap();
    std::thread::spawn(move || {
        for conn in l.incoming() {
            let Ok(mut conn) = conn else { continue };
            let updated = updated.clone();
            std::thread::spawn(move || {
                let mut r = std::io::BufReader::new(conn.try_clone().unwrap());
                while let Ok(Some(line)) = flow_ipc::read_line(&mut r) {
                    if flow_ipc::parse_request(&line) != Ok(Command::Subscribe) {
                        continue;
                    }
                    loop {
                        let mut status = Snapshot {
                            agent_version: "0.0.0".into(),
                            node_id: Some(1),
                            node_name: Some("laptop".into()),
                            hub_connected: true,
                            ..Default::default()
                        };
                        if updated.load(Ordering::SeqCst) {
                            status.update = UpdateInfo { tray_build: Some(NEW_BUILD.into()), ..Default::default() };
                        }
                        let line = flow_ipc::encode_line(&Reply::Status { status: Box::new(status) });
                        if conn.write_all(&line).is_err() {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
            });
        }
    });
}

fn alive(pid: u32) -> bool {
    let p = rustix::process::Pid::from_raw(pid as i32).unwrap();
    rustix::process::test_kill_process(p).is_ok()
}

fn wait_for<R>(what: &str, mut f: impl FnMut() -> Option<R>) -> R {
    let t0 = Instant::now();
    loop {
        if let Some(r) = f() {
            return r;
        }
        assert!(t0.elapsed() < T, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn exited(c: &mut Child) -> Option<()> {
    c.try_wait().unwrap().map(|_| ())
}

#[test]
fn an_updated_tray_starts_as_a_new_process_and_takes_over_the_lock() {
    let env = Env::new("restart");
    let updated = Arc::new(AtomicBool::new(false));
    fake_agent(&env.dir.join("agent.sock"), updated.clone());

    let mut old = env.tray(&[]).stderr(Stdio::null()).spawn().unwrap();
    let old_pid = old.id();
    wait_for("the first tray to take the lock", || (env.lock_pid() == Some(old_pid)).then_some(()));

    // The update installed another tray build.
    updated.store(true, Ordering::SeqCst);

    // The old process exits (it does not exec in place: no pid reuse) ...
    wait_for("the old tray to exit", || exited(&mut old));
    // ... and the new one holds the lock, under another pid, and is running.
    let new_pid = wait_for("the new tray to take the lock", || env.lock_pid().filter(|p| *p != old_pid));
    assert_ne!(new_pid, old_pid, "a new process, not an exec in place");
    assert!(alive(new_pid));
    // It stays: restarted once, not in a loop (it knows which build it is for).
    std::thread::sleep(Duration::from_millis(1500));
    assert!(alive(new_pid), "the new tray keeps running");
    assert_eq!(env.lock_pid(), Some(new_pid));

    // The new instance is the instance: a second `flow-tray` (login item,
    // Start menu entry) is refused by it.
    let out = env.tray(&[]).stderr(Stdio::piped()).output().unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("already running"), "{err}");
    assert_eq!(env.lock_pid(), Some(new_pid), "the refused one did not take over");

    // `--quit` finds the new tray through the lock file.
    assert!(env.tray(&["--quit"]).status().unwrap().success());
    wait_for("the new tray to quit", || (!alive(new_pid)).then_some(()));
}

#[test]
fn the_new_tray_waits_for_a_lock_that_is_still_held_and_is_not_refused() {
    // The order of the hand-over: the new process is started while the old
    // one still holds the lock, so it must wait for it instead of giving up
    // with "already running".
    let env = Env::new("handover");
    let updated = Arc::new(AtomicBool::new(false));
    fake_agent(&env.dir.join("agent.sock"), updated);

    let mut holder = env.tray(&[]).stderr(Stdio::null()).spawn().unwrap();
    wait_for("the holder", || (env.lock_pid() == Some(holder.id())).then_some(()));

    let mut next = env.tray(&[]).env("FLOW_TRAY_REEXEC", NEW_BUILD).stderr(Stdio::piped()).spawn().unwrap();
    std::thread::sleep(Duration::from_millis(800));
    assert!(next.try_wait().unwrap().is_none(), "still waiting for the lock");
    assert_eq!(env.lock_pid(), Some(holder.id()));

    // The holder lets go (it exits); the waiting one takes the lock.
    holder.kill().unwrap();
    holder.wait().unwrap();
    wait_for("the waiting tray to take over", || (env.lock_pid() == Some(next.id())).then_some(()));
    assert!(next.try_wait().unwrap().is_none(), "running, not refused");
    next.kill().unwrap();
    let out = next.wait_with_output().unwrap();
    assert!(!String::from_utf8_lossy(&out.stderr).contains("already running"));
}
