//! The agent's local IPC server end to end: hub + simulated agents, a real
//! Unix socket, the blocking `flow-ipc` client the tray uses.
#![cfg(unix)]

use flow_agent::ipc::{self, AgentInfo, BindError, Control, IpcContext};
use flow_agent::logbuf::LogBuffer;
use flow_ipc::{Client, Command, Reply, Snapshot};
use flow_itest::*;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

const T: Duration = Duration::from_secs(5);

fn ctx_for(a: &TestAgent, logs: LogBuffer) -> (IpcContext, mpsc::UnboundedReceiver<Control>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let ctx = IpcContext {
        agent: a.agent.observer(),
        info: Arc::new(AgentInfo {
            config_file: a.state_dir.join("agent.toml"),
            log_file: None,
            cfg: flow_agent::config::AgentConfig {
                hub: Some("127.0.0.1:7878".into()),
                state_dir: Some(a.state_dir.clone()),
                ..Default::default()
            },
        }),
        logs,
        control: tx,
    };
    (ctx, rx)
}

fn sock_path(tag: &str) -> PathBuf {
    temp_dir(tag).join("run").join("agent.sock")
}

/// Runs a blocking client call off the async runtime.
async fn call(path: &Path, cmd: Command) -> Reply {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || flow_ipc::client::request(&path, &cmd).expect("IPC request")).await.unwrap()
}

async fn status(path: &Path) -> Snapshot {
    match call(path, Command::Status).await {
        Reply::Status { status } => *status,
        r => panic!("{r:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn status_pause_logs_join_restart_over_the_socket() {
    init_logs();
    let hub = start_hub().await;
    let token = hub.join_token();
    let a = start_agent(&hub, AgentSpec::new(&token)).await;
    let b = start_agent(&hub, AgentSpec { addrs: vec![ip("10.10.15.11")], ..AgentSpec::new(&token) }).await;
    for x in [&a, &b] {
        assert!(x.agent.wait(T, |s| s.connected && s.online_peers.len() == 1).await);
    }
    let idb = b.agent.status().node_id.unwrap();
    assert!(a.agent.wait_link_path(idb, flow_proto::PathKind::P2p, T).await);

    let logs = LogBuffer::default();
    logs.push("first line");
    logs.push("second line");
    let (ctx, mut control) = ctx_for(&a, logs);
    let path = sock_path("ipc");
    let server = ipc::serve(&path, ctx).expect("serve");

    // The snapshot carries what the tray shows.
    let s = status(&path).await;
    assert!(s.hub_connected);
    assert_eq!(s.backend, "sim");
    assert_eq!(s.node_id, a.agent.status().node_id.map(|n| n.0));
    assert!(s.node_name.is_some());
    assert!(s.hub_fingerprint.is_some(), "pinned after joining");
    assert_eq!(s.dashboard_url.as_deref(), Some("http://127.0.0.1:7880"));
    assert_eq!(s.local_addrs, vec!["10.10.15.10".to_string()]);
    assert_eq!(s.peers.len(), 1);
    let p = &s.peers[0];
    assert_eq!(p.id, idb.0);
    assert!(p.online && p.enabled && !p.away);
    assert_eq!(p.path.as_deref(), Some("p2p"));
    assert!(!s.paused && !s.away && !s.revoked && s.problems.is_empty());
    assert!(!s.emergency_hotkey.is_empty());
    assert_eq!(s.pid, std::process::id());

    // A subscription pushes the change when sharing is paused.
    let sub_path = path.clone();
    let (sub_tx, mut sub_rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        let mut c = Client::connect(&sub_path).unwrap();
        c.send(&Command::Subscribe).unwrap();
        while let Ok(Reply::Status { status }) = c.recv() {
            if sub_tx.send(*status).is_err() {
                break;
            }
        }
    });
    let first = tokio::time::timeout(T, sub_rx.recv()).await.unwrap().unwrap();
    assert!(!first.paused);
    assert!(matches!(call(&path, Command::Pause).await, Reply::Ok { .. }));
    let paused = tokio::time::timeout(T, async {
        loop {
            let s = sub_rx.recv().await.unwrap();
            if s.paused {
                return s;
            }
        }
    })
    .await
    .expect("subscription reports the pause");
    assert!(!paused.away, "pause is not the network check");
    assert!(a.agent.wait(T, |s| s.paused).await);
    assert!(matches!(call(&path, Command::Resume).await, Reply::Ok { .. }));
    assert!(a.agent.wait(T, |s| !s.paused).await);
    assert!(!status(&path).await.paused);

    // Recent log lines.
    assert_eq!(call(&path, Command::Logs { max: 1 }).await, Reply::Logs { lines: vec!["second line".into()] });

    // Join: a bad token is refused right away, a good one goes to the binary.
    assert!(matches!(call(&path, Command::Join { token: "nope".into(), hub: None }).await, Reply::Error { .. }));
    assert!(control.try_recv().is_err());
    let r = call(&path, Command::Join { token: token.clone(), hub: Some(" 10.0.0.5 ".into()) }).await;
    assert!(matches!(r, Reply::Ok { .. }), "{r:?}");
    assert_eq!(control.recv().await, Some(Control::Join { token: token.clone(), hub: Some("10.0.0.5".into()) }));
    assert!(matches!(call(&path, Command::Restart).await, Reply::Ok { .. }));
    assert_eq!(control.recv().await, Some(Control::Restart));
    assert!(matches!(call(&path, Command::Shutdown).await, Reply::Ok { .. }));
    assert_eq!(control.recv().await, Some(Control::Shutdown));

    // Wrong version / unknown command: an error reply, the connection stays usable.
    let raw_path = path.clone();
    tokio::task::spawn_blocking(move || {
        use std::io::{BufRead, BufReader, Write};
        let mut s = std::os::unix::net::UnixStream::connect(&raw_path).unwrap();
        let mut r = BufReader::new(s.try_clone().unwrap());
        let mut line = String::new();
        for req in [r#"{"v":2,"cmd":"status"}"#, r#"{"v":1,"cmd":"format_disk"}"#, "garbage"] {
            s.write_all(format!("{req}\n").as_bytes()).unwrap();
            line.clear();
            r.read_line(&mut line).unwrap();
            assert!(line.contains(r#""type":"error""#), "{req} -> {line}");
        }
        assert!(line.contains("malformed"));
        s.write_all(b"{\"v\":1,\"cmd\":\"status\"}\n").unwrap();
        line.clear();
        r.read_line(&mut line).unwrap();
        assert!(line.starts_with(r#"{"v":1,"type":"status""#), "{line}");
    })
    .await
    .unwrap();

    server.close().await;
    assert!(!path.exists(), "socket removed when the server stops");
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn socket_is_private_and_single_instance() {
    init_logs();
    let hub = start_hub().await;
    let a = start_agent(&hub, AgentSpec::new(&hub.join_token())).await;
    let path = sock_path("perm");
    let dir = path.parent().unwrap().to_path_buf();

    // A stale socket from a crashed agent is replaced; a loose directory
    // is tightened.
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
    assert!(std::fs::symlink_metadata(&path).unwrap().file_type().is_socket());

    let server = ipc::serve(&path, ctx_for(&a, LogBuffer::default()).0).expect("stale socket replaced");
    assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
    let md = std::fs::metadata(&path).unwrap();
    assert!(md.file_type().is_socket());
    assert_eq!(md.permissions().mode() & 0o777, 0o600, "socket only for the owner");
    assert!(status(&path).await.hub_connected || !a.agent.status().connected);

    // A second agent of the same user is refused while the first answers.
    match ipc::serve(&path, ctx_for(&a, LogBuffer::default()).0) {
        Err(BindError::AlreadyRunning(p)) => assert_eq!(p, path),
        Err(e) => panic!("{e}"),
        Ok(_) => panic!("second server accepted"),
    }

    // A regular file at the socket path is not deleted.
    let other = dir.join("not-a-socket");
    std::fs::write(&other, b"keep").unwrap();
    assert!(matches!(ipc::serve(&other, ctx_for(&a, LogBuffer::default()).0), Err(BindError::Io(_))));
    assert_eq!(std::fs::read(&other).unwrap(), b"keep");

    // A directory that is a symlink (someone else's) is refused.
    let target = temp_dir("perm-target");
    let link = temp_dir("perm-link").join("run");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert!(matches!(ipc::serve(&link.join("agent.sock"), ctx_for(&a, LogBuffer::default()).0), Err(BindError::Io(_))));
    assert!(!target.join("agent.sock").exists());

    // Peers of other users are dropped before a byte is read.
    assert!(ipc::peer_allowed(flow_ipc::endpoint::current_uid(), flow_ipc::endpoint::current_uid()));
    assert!(!ipc::peer_allowed(flow_ipc::endpoint::current_uid() + 1, flow_ipc::endpoint::current_uid()));

    drop(server);
    assert!(!path.exists());
    hub.shutdown().await;
}
