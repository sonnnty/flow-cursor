//! Agent self-update end to end: a real hub carrying test agent builds,
//! simulated agents with the real updater, and the real Linux installer
//! working on a temporary home directory (`~/.local/bin`, unit file…). The
//! process restart is observed as the agent's restart request; the rollback
//! guard runs in-process with a recording restarter.

use flow_agent::backend::LocalEvent;
use flow_agent::update::guard::{self, GuardEnd, GuardOptions, Restarter};
use flow_agent::update::install::LinuxInstaller;
use flow_agent::update::state::{self, ServiceKind, Trial};
use flow_agent::update::{GuardLauncher, RestartRequest, UpdateOptions};
use flow_itest::*;
use flow_net::endpoint::{bind, client_config, server_config, TransportOpts, SNI};
use flow_net::frame::{read_frame, write_frame};
use flow_net::tls::{AnyClient, ServerPin};
use flow_proto::update::{FetchReply, FetchRequest, Release, ReleaseFile, UpdatePhase};
use flow_proto::{BuildId, StreamKind, UpdatePolicy, ALPN_HUB, PROTOCOL_VERSION};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const T: Duration = Duration::from_secs(10);

fn old() -> BuildId {
    BuildId::new("0.1.9", "old")
}

fn new() -> BuildId {
    BuildId::new("0.2.0", "new")
}

const UNIT: &[u8] = b"[Service]\nExecStart=@BINDIR@/flow-agent run\n";

/// The hub's builds of `new()`.
fn builds() -> PathBuf {
    agent_builds(
        &new(),
        &[
            ("flow-agent", b"new agent"),
            ("flow-tray", b"new tray"),
            ("flow-agent.service", UNIT),
            ("install.sh", b"#!/bin/sh\n"),
        ],
    )
}

/// A home directory as install.sh leaves it, with the old version.
fn test_home() -> PathBuf {
    let home = temp_dir("home");
    std::fs::create_dir_all(home.join(".local/bin")).unwrap();
    std::fs::create_dir_all(home.join(".config/systemd/user")).unwrap();
    std::fs::write(home.join(".local/bin/flow-agent"), "old agent").unwrap();
    std::fs::write(home.join(".local/bin/flow-tray"), "old tray").unwrap();
    std::fs::write(home.join(".config/systemd/user/flow-agent.service"), "old unit").unwrap();
    home
}

fn installer(home: &Path) -> LinuxInstaller {
    LinuxInstaller {
        exe: home.join(".local/bin/flow-agent"),
        home: home.to_path_buf(),
        data_home: home.join(".local/share"),
        config_home: home.join(".config"),
        systemd: false,
    }
}

type Launched = Arc<Mutex<Vec<PathBuf>>>;

/// Update options for an agent of build `current` installed in `home`;
/// the guards it would start are recorded.
fn update_opts(home: &Path, current: BuildId) -> (UpdateOptions, Launched) {
    let launched: Launched = Arc::default();
    let l = launched.clone();
    let guard: GuardLauncher = Arc::new(move |exe: &Path, _dir: &Path, _s: ServiceKind| {
        l.lock().unwrap().push(exe.to_path_buf());
        Ok(())
    });
    let opts = UpdateOptions {
        installer: Some(Arc::new(installer(home))),
        current,
        platform: "linux-x86_64".into(),
        quiet: Duration::from_millis(300),
        trial_timeout: Duration::from_secs(1),
        guard: Some(guard),
    };
    (opts, launched)
}

fn read(p: impl AsRef<Path>) -> String {
    String::from_utf8_lossy(&std::fs::read(p).unwrap()).to_string()
}

async fn next_restart(a: &mut TestAgent, within: Duration) -> Option<RestartRequest> {
    let rx = a.agent.restarts.as_mut().unwrap();
    tokio::time::timeout(within, rx.recv()).await.ok().flatten()
}

async fn node_json(hub: &flow_server::HubHandle, id: u32) -> serde_json::Value {
    let (code, body) = http(hub.http_addr.unwrap(), "GET", "/api/state", Some(ADMIN), None).await;
    assert_eq!(code, 200);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    v["nodes"].as_array().unwrap().iter().find(|n| n["id"] == id).cloned().expect("node in state")
}

async fn wait_node(hub: &flow_server::HubHandle, id: u32, f: impl Fn(&serde_json::Value) -> bool) -> serde_json::Value {
    let deadline = std::time::Instant::now() + T;
    loop {
        let n = node_json(hub, id).await;
        if f(&n) || std::time::Instant::now() > deadline {
            return n;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn phase(a: &TestAgent) -> UpdatePhase {
    a.agent.updater.status().phase
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_of_another_protocol_updates_in_update_only_mode_then_confirms() {
    init_logs();
    let hub = start_hub_with_builds(builds()).await;
    let token = hub.join_token();
    let home = test_home();
    let (opts, launched) = update_opts(&home, old());
    // Another protocol (an older agent, once this protocol is not the latest).
    let mut a = start_agent(
        &hub,
        AgentSpec { protocol: Some(PROTOCOL_VERSION + 1), update: Some(opts), ..AgentSpec::new(&token) },
    )
    .await;
    assert!(a.agent.wait(T, |s| s.update_only.is_some()).await, "update-only session");
    let s = a.agent.status();
    assert!(!s.connected && s.peers.is_empty() && s.layout_nodes == 0, "no input routing: {s:?}");
    let id = a.agent.agent_state().node_id.unwrap();
    let n = node_json(&hub, id).await;
    assert_eq!(n["status"], "update_only");
    assert_eq!(n["agent_version"], "0.1.9+old");
    assert_eq!(n["offered_version"], "0.2.0+new");

    // Policy auto (the default): download, verify, install, restart.
    let req = next_restart(&mut a, T).await.expect("restart request after installing");
    assert_eq!(req, RestartRequest { service: ServiceKind::Plain, unit_changed: true });
    let bin = home.join(".local/bin");
    assert_eq!(read(bin.join("flow-agent")), "new agent");
    assert_eq!(read(bin.join("flow-tray")), "new tray");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(bin.join("flow-agent")).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
    }
    let unit = read(home.join(".config/systemd/user/flow-agent.service"));
    assert_eq!(unit, format!("[Service]\nExecStart={}/flow-agent run\n", bin.display()), "@BINDIR@ filled in");
    // Only the files the installer needs were downloaded.
    let staged = state::staging_root(&a.state_dir).join("new");
    assert!(staged.join("flow-agent").exists() && !staged.join("install.sh").exists());
    // The previous version is kept and the trial recorded.
    let st = state::read(&a.state_dir);
    let trial = st.trial.clone().expect("trial");
    assert_eq!((trial.from.clone(), trial.to.clone()), (old(), new()));
    let saved_agent = trial.backups.iter().find(|b| b.dest == bin.join("flow-agent")).unwrap().saved.clone().unwrap();
    assert_eq!(read(&saved_agent), "old agent");
    assert_eq!(st.tray_build.as_deref(), Some("0.2.0+new"), "the tray learns to restart");
    // The guard runs the previous agent binary.
    assert_eq!(launched.lock().unwrap().as_slice(), [saved_agent]);
    let n = wait_node(&hub, id, |n| n["update"]["state"] == "installing").await;
    assert_eq!(n["update"]["state"], "installing", "{n}");

    // The new version starts (what main() does) and reaches the hub in a
    // full session: the trial ends, the hub sees it current.
    let dir = a.state_dir.clone();
    a.agent.shutdown().await;
    let (opts, _) = update_opts(&home, new());
    let launcher = opts.guard.clone().unwrap();
    flow_agent::update::on_start(&dir, &new(), "sim", &launcher);
    flow_agent::update::on_backend_ready(&dir, &new());
    assert_eq!(state::read(&dir).trial.unwrap().starts, 1);
    let b = start_agent(
        &hub,
        AgentSpec { token: None, state_dir: Some(dir.clone()), update: Some(opts), ..AgentSpec::new("") },
    )
    .await;
    assert!(b.agent.wait(T, |s| s.connected).await, "full session");
    assert!(eventually(T, || state::read(&dir).trial.is_none()).await, "trial confirmed");
    let n = wait_node(&hub, id, |n| n["update"]["state"] == "current").await;
    assert_eq!(n["status"], "online");
    assert_eq!(n["agent_version"], "0.2.0+new");
    assert_eq!(n["update"]["state"], "current");
    assert_eq!(phase(&b), UpdatePhase::Current);
    hub.shutdown().await;
}

/// A hub stand-in that answers fetch requests with whatever a scenario
/// (the requested file name) calls for.
async fn fake_hub() -> (SocketAddrPin, tokio::task::JoinHandle<()>) {
    let id = flow_net::Identity::generate("fake-hub").unwrap();
    let sc = server_config(&id, ALPN_HUB, Arc::new(AnyClient), TransportOpts::HUB).unwrap();
    let ep = bind("127.0.0.1:0".parse().unwrap(), Some(sc)).unwrap();
    let addr = ep.local_addr().unwrap();
    let task = tokio::spawn(async move {
        while let Some(inc) = ep.accept().await {
            let conn = inc.await.unwrap();
            tokio::spawn(async move {
                while let Ok((mut s, mut r)) = conn.accept_bi().await {
                    tokio::spawn(async move {
                        assert_eq!(read_frame::<_, StreamKind>(&mut r, 64).await.unwrap(), Some(StreamKind::Fetch));
                        let req = read_frame::<_, FetchRequest>(&mut r, 1024).await.unwrap().unwrap();
                        let (size, body): (u64, &[u8]) = match req.name.as_str() {
                            "hash" => (5, b"evil!"),
                            "size" => (6, b"good!!"),
                            "long" => (5, b"good!and more"),
                            "refused" => {
                                let _ = write_frame(&mut s, &FetchReply::Refused { reason: "no".into() }).await;
                                let _ = s.finish();
                                return;
                            }
                            _ => (5, b"good!"),
                        };
                        let _ = write_frame(&mut s, &FetchReply::Ok { size }).await;
                        let _ = s.write_all(body).await;
                        let _ = s.finish();
                        let _ = s.stopped().await;
                    });
                }
            });
        }
    });
    ((addr, id.fingerprint), task)
}

type SocketAddrPin = (std::net::SocketAddr, flow_proto::Fingerprint);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn downloads_refuse_wrong_hash_wrong_size_and_unlisted_files() {
    init_logs();
    let ((addr, fp), _task) = fake_hub().await;
    let me = flow_net::Identity::generate("agent").unwrap();
    let cc = client_config(&me, ALPN_HUB, ServerPin::Exact(fp), TransportOpts::HUB).unwrap();
    let ep = bind("127.0.0.1:0".parse().unwrap(), None).unwrap();
    let conn = ep.connect_with(cc, addr, SNI).unwrap().await.unwrap();
    let sha: [u8; 32] = Sha256::digest(b"good!").into();
    let file = |n: &str| ReleaseFile { name: n.into(), size: 5, sha256: sha };
    let release = Release {
        id: new(),
        platform: "linux-x86_64".into(),
        files: ["ok", "hash", "size", "long", "refused"].iter().map(|n| file(n)).collect(),
    };
    let dir = temp_dir("staging");
    let none = |_: u64, _: u64| {};
    let dl = |names: &[&str]| {
        let files: Vec<ReleaseFile> = names.iter().map(|n| file(n)).collect();
        let (conn, release, dir) = (conn.clone(), release.clone(), dir.clone());
        async move { flow_agent::update::fetch::download(&conn, &release, &files, &dir, &none).await }
    };
    let leftovers = || std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect::<Vec<_>>();

    dl(&["ok"]).await.expect("matching file");
    assert_eq!(std::fs::read(dir.join("ok")).unwrap(), b"good!");

    let e = dl(&["hash"]).await.unwrap_err().to_string();
    assert!(e.contains("SHA-256"), "{e}");
    let e = dl(&["size"]).await.unwrap_err().to_string();
    assert!(e.contains("announced 6 bytes"), "{e}");
    let e = dl(&["long"]).await.unwrap_err().to_string();
    assert!(e.contains("more than 5 bytes"), "{e}");
    let e = dl(&["refused"]).await.unwrap_err().to_string();
    assert!(e.contains("refused"), "{e}");
    // Not in the offered release: refused before asking the hub.
    let e = dl(&["../../.bashrc"]).await.unwrap_err().to_string();
    assert!(e.contains("not part of the offered release"), "{e}");
    let mut other = file("ok");
    other.size = 4;
    let files = vec![other];
    let e = flow_agent::update::fetch::download(&conn, &release, &files, &dir, &none).await.unwrap_err();
    assert!(e.to_string().contains("differs from the offered release"), "{e}");
    // Nothing but the good file was kept (no partial or bad files).
    assert_eq!(leftovers(), vec![std::ffi::OsString::from("ok")]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hub_serves_only_manifest_files() {
    init_logs();
    let b = builds();
    std::fs::write(b.join("linux-x86_64/not-listed"), b"secret").unwrap();
    let hub = start_hub_with_builds(b).await;
    let node = RawNode::join(&hub, &hub.join_token(), "raw").await;
    let fetch = |name: &str, build: &str| {
        let conn = node.conn.clone();
        let req = FetchRequest { build: build.into(), platform: "linux-x86_64".into(), name: name.into() };
        async move {
            let (mut s, mut r) = conn.open_bi().await.unwrap();
            write_frame(&mut s, &StreamKind::Fetch).await.unwrap();
            write_frame(&mut s, &req).await.unwrap();
            read_frame::<_, FetchReply>(&mut r, 4096).await.unwrap().unwrap()
        }
    };
    assert_eq!(fetch("flow-agent", "new").await, FetchReply::Ok { size: 9 });
    for (name, build) in [
        ("not-listed", "new"),
        ("manifest.json", "new"),
        ("../manifest.json", "new"),
        ("../../state.json", "new"),
        ("flow-agent", "other-build"),
    ] {
        assert!(matches!(fetch(name, build).await, FetchReply::Refused { .. }), "{name} of {build}");
    }
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tampered_file_is_refused_and_not_retried() {
    init_logs();
    let b = builds();
    let hub = start_hub_with_builds(b.clone()).await;
    // Same length, other bytes, after the hub verified and announced it.
    std::fs::write(b.join("linux-x86_64/flow-agent"), b"bad agent").unwrap();
    let home = test_home();
    let (opts, _) = update_opts(&home, old());
    let mut a = start_agent(&hub, AgentSpec { update: Some(opts), ..AgentSpec::new(&hub.join_token()) }).await;
    assert!(a.agent.wait(T, |s| s.connected).await);
    let id = a.agent.status().node_id.unwrap().0;
    let n = wait_node(&hub, id, |n| n["update"]["failed"].is_object()).await;
    let reason = n["update"]["failed"]["reason"].as_str().unwrap().to_string();
    assert!(reason.contains("SHA-256"), "{n}");
    assert_eq!(n["update"]["failed"]["rolled_back"], false);
    assert_eq!(n["update"]["state"], "available", "not retried automatically");
    assert!(next_restart(&mut a, Duration::from_millis(1500)).await.is_none());
    assert_eq!(read(home.join(".local/bin/flow-agent")), "old agent", "nothing installed");
    assert!(matches!(phase(&a), UpdatePhase::Available { .. }));
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn policies_off_notify_auto_and_update_now() {
    init_logs();
    let hub = start_hub_with_builds(builds()).await;
    hub.hub.lock().p.settings.update_policy = UpdatePolicy::Off;
    let token = hub.join_token();
    let home_a = test_home();
    let (opts, _) = update_opts(&home_a, old());
    let mut a = start_agent(&hub, AgentSpec { update: Some(opts), ..AgentSpec::new(&token) }).await;
    assert!(a.agent.wait(T, |s| s.connected).await);
    let ida = a.agent.status().node_id.unwrap().0;

    // off: available, nothing else.
    let n = wait_node(&hub, ida, |n| n["update"]["state"] == "available").await;
    assert_eq!(n["effective_update_policy"], "off");
    assert!(next_restart(&mut a, Duration::from_millis(1200)).await.is_none());
    assert_eq!(a.agent.updater.status().policy, Some(UpdatePolicy::Off));

    // notify (per-machine override, pushed live): the tray offers Update now.
    let web = hub.http_addr.unwrap();
    let (code, _) =
        http(web, "PATCH", &format!("/api/nodes/{ida}"), Some(ADMIN), Some(r#"{"update_policy":"notify"}"#)).await;
    assert_eq!(code, 204);
    assert!(eventually(T, || a.agent.updater.status().policy == Some(UpdatePolicy::Notify)).await);
    let info = flow_agent::ipc::update_info(&a.agent.updater.status());
    assert!(
        info.can_update_now && info.policy == "notify" && info.available.as_deref() == Some("0.2.0+new"),
        "{info:?}"
    );
    assert!(next_restart(&mut a, Duration::from_millis(800)).await.is_none(), "notify does not install");

    // Update now from the dashboard.
    let (code, _) = http(web, "POST", &format!("/api/nodes/{ida}/update"), Some(ADMIN), None).await;
    assert_eq!(code, 202);
    assert!(next_restart(&mut a, T).await.is_some(), "Update now installs");
    assert_eq!(read(home_a.join(".local/bin/flow-agent")), "new agent");

    // auto (global): another machine updates on its own.
    let (code, _) = http(web, "GET", "/api/settings", Some(ADMIN), None).await;
    assert_eq!(code, 200);
    {
        let mut st = hub.hub.lock();
        st.p.settings.update_policy = UpdatePolicy::Auto;
        st.broadcast_offers();
    }
    let home_b = test_home();
    let (opts, _) = update_opts(&home_b, old());
    let mut b =
        start_agent(&hub, AgentSpec { update: Some(opts), addrs: vec![ip("10.10.15.11")], ..AgentSpec::new(&token) })
            .await;
    assert!(next_restart(&mut b, T).await.is_some(), "auto installs");
    assert_eq!(read(home_b.join(".local/bin/flow-agent")), "new agent");

    // Unsupported installation (a .deb in /usr/bin): reported, nothing done.
    let (mut opts, _) = update_opts(&home_b, old());
    opts.installer = Some(Arc::new(LinuxInstaller { exe: "/usr/bin/flow-agent".into(), ..installer(&home_b) }));
    let mut c =
        start_agent(&hub, AgentSpec { update: Some(opts), addrs: vec![ip("10.10.15.12")], ..AgentSpec::new(&token) })
            .await;
    assert!(c.agent.wait(T, |s| s.connected).await);
    let idc = c.agent.status().node_id.unwrap().0;
    let n = wait_node(&hub, idc, |n| n["update"]["state"] == "unsupported").await;
    assert!(n["update"]["detail"].as_str().unwrap().contains("apt"), "{n}");
    assert!(next_restart(&mut c, Duration::from_millis(800)).await.is_none());
    // Update now cannot help either.
    c.agent.updater.update_now();
    assert!(next_restart(&mut c, Duration::from_millis(800)).await.is_none());
    hub.shutdown().await;
}

/// Two machines side by side; `a` runs `old()` and is offered `new()`,
/// `b` already runs `new()`. Policy off until the test is ready.
async fn pair(hub: &flow_server::HubHandle) -> (TestAgent, TestAgent, PathBuf) {
    hub.hub.lock().p.settings.update_policy = UpdatePolicy::Off;
    let token = hub.join_token();
    let home = test_home();
    let (opts, _) = update_opts(&home, old());
    let a = start_agent(hub, AgentSpec { update: Some(opts), ..AgentSpec::new(&token) }).await;
    // Enrolled first = placed first: b ends up to the right of a.
    assert!(a.agent.wait(T, |s| s.connected).await);
    let (opts_b, _) = update_opts(&test_home(), new());
    let b =
        start_agent(hub, AgentSpec { update: Some(opts_b), addrs: vec![ip("10.10.15.11")], ..AgentSpec::new(&token) })
            .await;
    for x in [&a, &b] {
        assert!(x.agent.wait(T, |s| s.connected && s.online_peers.len() == 1 && s.layout_nodes == 2).await);
    }
    let (ida, idb) = (a.agent.status().node_id.unwrap(), b.agent.status().node_id.unwrap());
    assert!(a.agent.wait_link_path(idb, flow_proto::PathKind::P2p, T).await);
    assert!(b.agent.wait_link_path(ida, flow_proto::PathKind::P2p, T).await);
    (a, b, home)
}

fn allow_auto(hub: &flow_server::HubHandle) {
    let mut st = hub.hub.lock();
    st.p.settings.update_policy = UpdatePolicy::Auto;
    st.broadcast_offers();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_update_while_controlling_or_holding_keys() {
    init_logs();
    let hub = start_hub_with_builds(builds()).await;
    let (mut a, b, home) = pair(&hub).await;
    let idb = b.agent.status().node_id.unwrap();
    // a's pointer crosses into b: a controls b.
    a.sim.push_edge(flow_layout::Side::Right, 1919, 540);
    assert!(a.agent.wait(T, |s| s.controlling == Some(idb)).await);
    a.sim.key(flow_keymap::keys::A, true);
    assert!(a.agent.wait(T, |s| s.keys_held).await, "a key is held");

    allow_auto(&hub);
    assert!(eventually(T, || matches!(phase(&a), UpdatePhase::Ready { .. })).await, "downloaded, waits");
    assert!(next_restart(&mut a, Duration::from_millis(1500)).await.is_none(), "not while controlling");
    assert_eq!(read(home.join(".local/bin/flow-agent")), "old agent");

    // Back to its own screen, key released: installs after the quiet time.
    a.sim.key(flow_keymap::keys::A, false);
    a.sim.emit(LocalEvent::CaptureEnded);
    assert!(a.agent.wait(T, |s| s.controlling.is_none() && !s.keys_held).await);
    assert!(next_restart(&mut a, T).await.is_some(), "installs once idle");
    assert_eq!(read(home.join(".local/bin/flow-agent")), "new agent");
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_update_while_controlled() {
    init_logs();
    let hub = start_hub_with_builds(builds()).await;
    let (mut a, b, _home) = pair(&hub).await;
    let ida = a.agent.status().node_id.unwrap();
    // b's pointer crosses into a (b is right of a): a is controlled.
    b.sim.push_edge(flow_layout::Side::Left, 0, 540);
    assert!(b.agent.wait(T, |s| s.controlling == Some(ida)).await);
    assert!(a.agent.wait(T, |s| !s.controlled_by.is_empty()).await);
    allow_auto(&hub);
    assert!(eventually(T, || matches!(phase(&a), UpdatePhase::Ready { .. })).await);
    assert!(next_restart(&mut a, Duration::from_millis(1500)).await.is_none(), "not while controlled");
    b.sim.emit(LocalEvent::CaptureEnded);
    assert!(a.agent.wait(T, |s| s.controlled_by.is_empty()).await);
    assert!(next_restart(&mut a, T).await.is_some(), "installs once idle");
    hub.shutdown().await;
}

/// Records what the guard asked for instead of restarting processes.
#[derive(Default)]
struct RecordRestart(Mutex<Vec<Trial>>);

impl Restarter for RecordRestart {
    fn restart(&self, t: &Trial) -> anyhow::Result<()> {
        self.0.lock().unwrap().push(t.clone());
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rollback_when_the_new_version_does_not_reach_the_hub_and_no_retry() {
    init_logs();
    let hub = start_hub_with_builds(builds()).await;
    let home = test_home();
    let (opts, _) = update_opts(&home, old());
    let mut a = start_agent(&hub, AgentSpec { update: Some(opts), ..AgentSpec::new(&hub.join_token()) }).await;
    assert!(a.agent.wait(T, |s| s.connected).await);
    let id = a.agent.status().node_id.unwrap().0;
    assert!(next_restart(&mut a, T).await.is_some());
    assert_eq!(read(home.join(".local/bin/flow-agent")), "new agent");
    let dir = a.state_dir.clone();
    a.agent.shutdown().await;

    // The new version starts and gets its backend, but never reaches the hub.
    let (opts_new, _) = update_opts(&home, new());
    flow_agent::update::on_start(&dir, &new(), "sim", opts_new.guard.as_ref().unwrap());
    flow_agent::update::on_backend_ready(&dir, &new());
    let restarter = Arc::new(RecordRestart::default());
    let (d, r) = (dir.clone(), restarter.clone());
    let opts = GuardOptions { poll: Duration::from_millis(100), ..GuardOptions::default() };
    let end = tokio::task::spawn_blocking(move || guard::run(&d, &opts, &*r)).await.unwrap().unwrap();
    let GuardEnd::RolledBack(reason) = end else { panic!("{end:?}") };
    assert!(reason.contains("did not reach the hub within 1 s"), "{reason}");
    assert_eq!(restarter.0.lock().unwrap().len(), 1, "restarted once");

    // Previous files back, failure recorded, tray told to go back.
    assert_eq!(read(home.join(".local/bin/flow-agent")), "old agent");
    assert_eq!(read(home.join(".local/bin/flow-tray")), "old tray");
    assert_eq!(read(home.join(".config/systemd/user/flow-agent.service")), "old unit");
    let st = state::read(&dir);
    assert!(st.trial.is_none());
    let f = st.failed.clone().expect("failure recorded");
    assert_eq!(f.target, new());
    assert!(f.rolled_back);
    assert_eq!(st.tray_build.as_deref(), Some("0.1.9+old"));
    // A second guard has nothing to do. (Retried briefly: a child process
    // another test spawns at that moment can hold a copy of the lock's file
    // descriptor until its exec closes it.)
    let opts = GuardOptions { poll: Duration::from_millis(100), ..GuardOptions::default() };
    let mut end = guard::run(&dir, &opts, &RecordRestart::default()).unwrap();
    for _ in 0..20 {
        if end != GuardEnd::AlreadyRunning {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        end = guard::run(&dir, &opts, &RecordRestart::default()).unwrap();
    }
    assert_eq!(end, GuardEnd::Done);

    // The previous version runs again: it reports the failure and does not
    // try the same build again on its own.
    let (opts, _) = update_opts(&home, old());
    let mut a =
        start_agent(&hub, AgentSpec { token: None, state_dir: Some(dir), update: Some(opts), ..AgentSpec::new("") })
            .await;
    assert!(a.agent.wait(T, |s| s.connected).await);
    let n = wait_node(&hub, id, |n| n["update"]["failed"].is_object()).await;
    assert_eq!(n["agent_version"], "0.1.9+old");
    assert_eq!(n["update"]["failed"]["target"], "0.2.0+new");
    assert_eq!(n["update"]["failed"]["rolled_back"], true);
    assert!(n["update"]["failed"]["reason"].as_str().unwrap().contains("did not reach the hub"));
    assert_eq!(n["update"]["state"], "available");
    assert!(next_restart(&mut a, Duration::from_millis(2000)).await.is_none(), "no retry loop");
    // An explicit Update now tries again.
    a.agent.updater.update_now();
    assert!(next_restart(&mut a, T).await.is_some(), "manual retry");
    hub.shutdown().await;
}
