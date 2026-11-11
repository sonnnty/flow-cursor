//! Background thread that follows the agent over IPC (one subscription)
//! and reports every new state. Commands go over their own short
//! connections ([`crate::actions`]): a Windows pipe opened as a plain file
//! serialises reads and writes, so one connection must not be read and
//! written from two threads at once.

use crate::model::AgentState;
use flow_ipc::{Client, Command, Reply};
use std::path::PathBuf;
use std::time::Duration;

/// How long to wait between connection attempts.
pub const RETRY: Duration = Duration::from_secs(2);

/// Calls `on_state` with every state of the agent at `endpoint`, forever.
pub fn spawn(endpoint: PathBuf, on_state: impl Fn(AgentState) + Send + 'static) {
    std::thread::Builder::new()
        .name("ipc-watch".into())
        .spawn(move || loop {
            let err = follow(&endpoint, &on_state);
            on_state(AgentState::Unreachable(err.to_string()));
            std::thread::sleep(RETRY);
        })
        .expect("spawning the IPC thread");
}

/// Subscribes and forwards snapshots until the connection fails.
fn follow(endpoint: &std::path::Path, on_state: &impl Fn(AgentState)) -> std::io::Error {
    let mut c = match Client::connect(endpoint) {
        Ok(c) => c,
        Err(e) => return e,
    };
    // Three missed heartbeats: the agent hangs.
    if let Err(e) = c.set_read_timeout(Some(Duration::from_millis(flow_ipc::SUBSCRIBE_HEARTBEAT_MS * 3))) {
        return e;
    }
    if let Err(e) = c.send(&Command::Subscribe) {
        return e;
    }
    loop {
        match c.recv() {
            Ok(Reply::Status { status }) => on_state(AgentState::Running(status)),
            Ok(Reply::Error { message }) => return std::io::Error::other(message),
            Ok(_) => {}
            Err(e) => return e,
        }
    }
}
