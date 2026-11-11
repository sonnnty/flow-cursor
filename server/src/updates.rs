//! The agent builds a hub carries (built into its image by
//! `build/build.sh`) and the download streams that serve them.
//!
//! Layout of the directory (`FLOW_AGENT_BUILDS`, image default
//! `/usr/share/flowcursor/agent-builds`):
//!
//! ```text
//! manifest.json
//! linux-x86_64/flow-agent, flow-tray, flow-agent.service, *.desktop, icons
//! windows-x86_64/flow-agent.exe, flow-tray.exe, FlowCursor-setup-<version>.exe
//! ```
//!
//! At startup every file is checked against the manifest (size and
//! SHA-256); a platform with a missing or different file is not offered.
//! Agents get name, size and hash of every file in their update offer over
//! the authenticated control stream, ask for files by name on a fetch
//! stream, and the hub serves only files listed in its manifest, from the
//! directory of the requested platform. Nothing here is reachable over
//! HTTP.

use anyhow::{bail, Context, Result};
use flow_net::frame::{read_frame, write_frame};
use flow_net::quinn;
use flow_proto::update::{valid_file_name, FetchReply, FetchRequest, Release, ReleaseFile};
use flow_proto::BuildId;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use tokio::io::AsyncReadExt;
use tracing::{debug, error, info, warn};

pub const MANIFEST_FILE: &str = "manifest.json";
pub const MANIFEST_FORMAT: u32 = 1;
/// Where the image keeps the agent builds.
pub const DEFAULT_DIR: &str = "/usr/share/flowcursor/agent-builds";

/// `manifest.json`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub format: u32,
    pub version: String,
    pub build: String,
    #[serde(default)]
    pub commit: String,
    pub platforms: BTreeMap<String, PlatformFiles>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlatformFiles {
    pub files: Vec<ManifestFile>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ManifestFile {
    pub name: String,
    pub size: u64,
    /// Hex SHA-256.
    pub sha256: String,
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

/// SHA-256 and size of a file.
pub fn hash_file(path: &Path) -> Result<([u8; 32], u64)> {
    let mut f = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut n = 0u64;
    loop {
        let r = f.read(&mut buf)?;
        if r == 0 {
            break;
        }
        h.update(&buf[..r]);
        n += r as u64;
    }
    Ok((h.finalize().into(), n))
}

/// Platform keys: `<os>-<arch>`, lowercase.
fn valid_platform(p: &str) -> bool {
    !p.is_empty()
        && p.len() <= 32
        && p.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "-_".contains(c))
}

/// Writes `manifest.json` for the files in `<dir>/<platform>/` (what
/// `flow-server --write-agent-manifest` does at build time).
pub fn write_manifest(dir: &Path, id: &BuildId, commit: &str) -> Result<Manifest> {
    let mut platforms = BTreeMap::new();
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .collect::<std::io::Result<_>>()?;
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        if !e.file_type()?.is_dir() {
            continue;
        }
        let platform = e.file_name().to_string_lossy().to_string();
        if !valid_platform(&platform) {
            bail!("{platform:?} is not a platform name like linux-x86_64");
        }
        let mut names: Vec<String> = std::fs::read_dir(e.path())?
            .map(|f| f.map(|f| f.file_name().to_string_lossy().to_string()))
            .collect::<std::io::Result<_>>()?;
        names.sort();
        let mut files = Vec::new();
        for name in names {
            if !valid_file_name(&name) {
                bail!("{platform}/{name}: not a plain file name");
            }
            let path = e.path().join(&name);
            if !path.is_file() {
                bail!("{}: not a file", path.display());
            }
            let (sha, size) = hash_file(&path)?;
            files.push(ManifestFile { name, size, sha256: hex(&sha) });
        }
        platforms.insert(platform, PlatformFiles { files });
    }
    let m = Manifest {
        format: MANIFEST_FORMAT,
        version: id.version.clone(),
        build: id.build.clone(),
        commit: commit.to_string(),
        platforms,
    };
    std::fs::write(dir.join(MANIFEST_FILE), serde_json::to_vec_pretty(&m)?)?;
    Ok(m)
}

/// The verified agent builds of this hub.
#[derive(Debug)]
pub struct AgentBuilds {
    pub dir: PathBuf,
    pub id: BuildId,
    pub commit: String,
    /// Platforms whose files all match the manifest.
    releases: BTreeMap<String, Release>,
    /// Platforms left out, and why.
    pub broken: BTreeMap<String, String>,
}

impl AgentBuilds {
    /// Loads and verifies `<dir>/manifest.json`. `Ok(None)` when the
    /// directory or the manifest does not exist (a hub built without agent
    /// builds, e.g. `docker compose up --build`).
    pub fn load(dir: &Path) -> Result<Option<AgentBuilds>> {
        let mpath = dir.join(MANIFEST_FILE);
        if !mpath.exists() {
            return Ok(None);
        }
        let m: Manifest =
            serde_json::from_slice(&std::fs::read(&mpath)?).with_context(|| format!("parsing {}", mpath.display()))?;
        if m.format != MANIFEST_FORMAT {
            bail!("{}: unknown manifest format {}", mpath.display(), m.format);
        }
        let id = BuildId::new(m.version.clone(), m.build.clone());
        let mut releases = BTreeMap::new();
        let mut broken = BTreeMap::new();
        for (platform, pf) in &m.platforms {
            match Self::verify(dir, &id, platform, pf) {
                Ok(r) => {
                    releases.insert(platform.clone(), r);
                }
                Err(e) => {
                    error!(%platform, "agent build not offered: {e:#}");
                    broken.insert(platform.clone(), format!("{e:#}"));
                }
            }
        }
        Ok(Some(AgentBuilds { dir: dir.to_path_buf(), id, commit: m.commit, releases, broken }))
    }

    fn verify(dir: &Path, id: &BuildId, platform: &str, pf: &PlatformFiles) -> Result<Release> {
        if !valid_platform(platform) {
            bail!("invalid platform name");
        }
        let mut files = Vec::new();
        for f in &pf.files {
            let sha = unhex32(&f.sha256).with_context(|| format!("{}: sha256 is not 64 hex digits", f.name))?;
            files.push(ReleaseFile { name: f.name.clone(), size: f.size, sha256: sha });
        }
        let release = Release { id: id.clone(), platform: platform.to_string(), files };
        release.validate().map_err(anyhow::Error::msg)?;
        for f in &release.files {
            let path = dir.join(platform).join(&f.name);
            let (sha, size) = hash_file(&path)?;
            if size != f.size || sha != f.sha256 {
                bail!("{} does not match the manifest (size {size}, expected {})", path.display(), f.size);
            }
        }
        Ok(release)
    }

    /// The release for `platform`, if this hub carries one.
    pub fn release(&self, platform: &str) -> Option<Release> {
        self.releases.get(platform).cloned()
    }

    pub fn releases(&self) -> impl Iterator<Item = &Release> {
        self.releases.values()
    }

    /// Opens a file for a fetch request. Only files of the manifest's build
    /// and platform, by their manifest name, from this hub's directory.
    async fn open(&self, req: &FetchRequest) -> Result<(tokio::fs::File, u64), String> {
        if req.build != self.id.build {
            return Err(format!("this hub carries build {}, not {}", self.id.build, req.build));
        }
        let release = self.releases.get(&req.platform).ok_or("no build for this platform")?;
        let file = release.file(&req.name).ok_or("no such file in the manifest")?;
        // The path comes from the verified manifest entry, never from the
        // request (whose name only selected the entry).
        let path = self.dir.join(&release.platform).join(&file.name);
        let f = tokio::fs::File::open(&path).await.map_err(|e| format!("opening {}: {e}", file.name))?;
        let len = f.metadata().await.map_err(|e| e.to_string())?.len();
        if len != file.size {
            return Err(format!("{} changed on the hub since it started", file.name));
        }
        Ok((f, file.size))
    }
}

/// Opens the agent builds at `dir` and logs what the hub offers.
pub fn load_logged(dir: &Path) -> Option<std::sync::Arc<AgentBuilds>> {
    match AgentBuilds::load(dir) {
        Ok(Some(b)) => {
            let platforms: Vec<&str> = b.releases.keys().map(String::as_str).collect();
            info!(dir = %dir.display(), build = %b.id, ?platforms, "agent builds for self-update loaded");
            Some(std::sync::Arc::new(b))
        }
        Ok(None) => {
            info!(dir = %dir.display(), "no agent builds in this hub (agents are not offered updates)");
            None
        }
        Err(e) => {
            error!("agent builds unusable, agents are not offered updates: {e:#}");
            None
        }
    }
}

/// Serves one fetch stream (after its `StreamKind::Fetch` frame).
pub async fn serve_fetch(
    builds: Option<std::sync::Arc<AgentBuilds>>,
    node: flow_proto::NodeId,
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
) -> Result<()> {
    // Input on the relay stream of the same connection goes first.
    let _ = send.set_priority(-1);
    let req = read_frame::<_, FetchRequest>(&mut recv, 1024).await?.context("no fetch request")?;
    let opened = match &builds {
        Some(b) => b.open(&req).await,
        None => Err("this hub carries no agent builds".to_string()),
    };
    let (mut file, size) = match opened {
        Ok(v) => v,
        Err(reason) => {
            warn!(%node, file = %req.name, platform = %req.platform, "fetch refused: {reason}");
            write_frame(&mut send, &FetchReply::Refused { reason }).await?;
            let _ = send.finish();
            return Ok(());
        }
    };
    info!(%node, file = %req.name, platform = %req.platform, size, "serving agent update file");
    write_frame(&mut send, &FetchReply::Ok { size }).await?;
    let mut left = size;
    let mut buf = vec![0u8; 64 * 1024];
    while left > 0 {
        let want = buf.len().min(left as usize);
        let n = file.read(&mut buf[..want]).await?;
        if n == 0 {
            bail!("{} ended early", req.name);
        }
        send.write_all(&buf[..n]).await?;
        left -= n as u64;
    }
    send.finish()?;
    // Keep the stream until the agent has read everything.
    let _ = tokio::time::timeout(std::time::Duration::from_secs(30), send.stopped()).await;
    debug!(%node, file = %req.name, "fetch done");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("flow-builds-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn manifest_roundtrip_and_verification() {
        let d = tmp();
        std::fs::create_dir_all(d.join("linux-x86_64")).unwrap();
        std::fs::create_dir_all(d.join("windows-x86_64")).unwrap();
        std::fs::write(d.join("linux-x86_64/flow-agent"), b"agent-bytes").unwrap();
        std::fs::write(d.join("linux-x86_64/flow-tray"), b"tray").unwrap();
        std::fs::write(d.join("windows-x86_64/flow-agent.exe"), b"exe").unwrap();
        let id = BuildId::new("0.2.0", "9.gabc");
        let m = write_manifest(&d, &id, "abc").unwrap();
        assert_eq!(m.platforms["linux-x86_64"].files.len(), 2);
        let b = AgentBuilds::load(&d).unwrap().unwrap();
        let r = b.release("linux-x86_64").unwrap();
        assert_eq!(r.id, id);
        assert_eq!(r.file("flow-agent").unwrap().size, 11);
        assert!(b.release("macos-aarch64").is_none());
        // A file that no longer matches takes its platform out.
        std::fs::write(d.join("windows-x86_64/flow-agent.exe"), b"EXE").unwrap();
        let b = AgentBuilds::load(&d).unwrap().unwrap();
        assert!(b.release("windows-x86_64").is_none());
        assert!(b.broken.contains_key("windows-x86_64"));
        assert!(b.release("linux-x86_64").is_some());
        // No manifest: no builds, not an error.
        assert!(AgentBuilds::load(&d.join("nothing")).unwrap().is_none());
        // Names that are not plain files are refused at build time.
        std::fs::create_dir_all(d.join("linux-x86_64/sub")).unwrap();
        assert!(write_manifest(&d, &id, "").is_err());
        std::fs::remove_dir_all(d).ok();
    }

    #[tokio::test]
    async fn open_only_serves_manifest_files() {
        let d = tmp();
        std::fs::create_dir_all(d.join("linux-x86_64")).unwrap();
        std::fs::write(d.join("linux-x86_64/flow-agent"), b"agent").unwrap();
        let id = BuildId::new("0.2.0", "9");
        write_manifest(&d, &id, "").unwrap();
        // Present on disk but not in the manifest.
        std::fs::write(d.join("linux-x86_64/secret"), b"x").unwrap();
        std::fs::write(d.join("hub.key"), b"x").unwrap();
        let b = AgentBuilds::load(&d).unwrap().unwrap();
        let req = |build: &str, platform: &str, name: &str| FetchRequest {
            build: build.into(),
            platform: platform.into(),
            name: name.into(),
        };
        assert_eq!(b.open(&req("9", "linux-x86_64", "flow-agent")).await.unwrap().1, 5);
        for bad in [
            req("9", "linux-x86_64", "secret"),
            req("9", "linux-x86_64", "../hub.key"),
            req("9", "linux-x86_64", "manifest.json"),
            req("9", "..", "hub.key"),
            req("8", "linux-x86_64", "flow-agent"),
            req("9", "windows-x86_64", "flow-agent"),
        ] {
            assert!(b.open(&bad).await.is_err(), "{bad:?}");
        }
        std::fs::remove_dir_all(d).ok();
    }
}
