//! Downloads update files from the hub over the agent's authenticated QUIC
//! connection and verifies them before anything else sees them.
//!
//! For every file: the size and SHA-256 come from the hub's offer on the
//! control stream (never from the download stream); the hub's announced
//! size must equal it before anything is written; bytes go to
//! `<name>.part`, at most that many; the hash is checked; only then is the
//! file renamed to `<name>` in the staging directory.

use anyhow::{anyhow, bail, Context, Result};
use flow_net::frame::{read_frame, write_frame};
use flow_net::quinn::Connection;
use flow_proto::update::{valid_file_name, FetchReply, FetchRequest, Release, ReleaseFile};
use flow_proto::StreamKind;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::AsyncWriteExt;

/// Called with (bytes done, bytes total) while downloading.
pub type Progress<'a> = &'a (dyn Fn(u64, u64) + Send + Sync);

/// A download stream must deliver something within this time.
const STALL: Duration = Duration::from_secs(30);

/// The staging directory of a build (`staging/<build>`); the build id is
/// checked to be a plain name before it becomes a path component.
pub fn staging_dir(root: &Path, build: &str) -> Result<PathBuf> {
    if !valid_file_name(build) {
        bail!("build id {build:?} is not a plain name");
    }
    Ok(root.join(build))
}

/// SHA-256 of a file on disk.
pub fn sha256_file(path: &Path) -> std::io::Result<[u8; 32]> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            return Ok(h.finalize().into());
        }
        h.update(&buf[..n]);
    }
}

/// The staged file matches the offer (size and hash).
pub fn staged_ok(dir: &Path, f: &ReleaseFile) -> bool {
    let p = dir.join(&f.name);
    std::fs::metadata(&p).is_ok_and(|m| m.len() == f.size) && sha256_file(&p).is_ok_and(|h| h == f.sha256)
}

/// Downloads the `files` of `release` into `dir` (skipping files already
/// staged and verified). Errors name the file and what was wrong.
pub async fn download(
    conn: &Connection,
    release: &Release,
    files: &[ReleaseFile],
    dir: &Path,
    progress: Progress<'_>,
) -> Result<()> {
    release.validate().map_err(|e| anyhow!("the hub's offer is invalid: {e}"))?;
    tokio::fs::create_dir_all(dir).await.with_context(|| format!("creating {}", dir.display()))?;
    let total: u64 = files.iter().map(|f| f.size).sum();
    let mut done = 0u64;
    progress(done, total);
    for f in files {
        // Only what the offer lists, by the offer's name, size and hash.
        let offered = release.file(&f.name).ok_or_else(|| anyhow!("{} is not part of the offered release", f.name))?;
        if offered != f {
            bail!("{} differs from the offered release", f.name);
        }
        if staged_ok(dir, f) {
            done += f.size;
            progress(done, total);
            continue;
        }
        fetch_one(conn, release, f, dir, &mut |n| progress(done + n, total)).await?;
        done += f.size;
        progress(done, total);
    }
    Ok(())
}

async fn fetch_one(
    conn: &Connection,
    release: &Release,
    f: &ReleaseFile,
    dir: &Path,
    progress: &mut (dyn FnMut(u64) + Send),
) -> Result<()> {
    let (mut send, mut recv) = conn.open_bi().await.context("opening a download stream")?;
    write_frame(&mut send, &StreamKind::Fetch).await?;
    let req =
        FetchRequest { build: release.id.build.clone(), platform: release.platform.clone(), name: f.name.clone() };
    write_frame(&mut send, &req).await?;
    let _ = send.finish();
    let reply = tokio::time::timeout(STALL, read_frame::<_, FetchReply>(&mut recv, 4096))
        .await
        .map_err(|_| anyhow!("{}: the hub did not answer", f.name))??
        .ok_or_else(|| anyhow!("{}: the hub closed the download", f.name))?;
    let size = match reply {
        FetchReply::Ok { size } => size,
        FetchReply::Refused { reason } => bail!("{}: the hub refused the download: {reason}", f.name),
    };
    // Size check before writing anything.
    if size != f.size {
        let _ = recv.stop(0u32.into());
        bail!("{}: the hub announced {size} bytes, the offer said {}", f.name, f.size);
    }
    let part = dir.join(format!("{}.part", f.name));
    let mut out = tokio::fs::File::create(&part).await.with_context(|| format!("creating {}", part.display()))?;
    let mut h = Sha256::new();
    let mut got = 0u64;
    let res: Result<()> = async {
        loop {
            let chunk = tokio::time::timeout(STALL, recv.read_chunk(64 * 1024, true))
                .await
                .map_err(|_| anyhow!("{}: download stalled", f.name))?
                .with_context(|| format!("{}: download interrupted", f.name))?;
            let Some(chunk) = chunk else { break };
            got += chunk.bytes.len() as u64;
            if got > f.size {
                bail!("{}: the hub sent more than {} bytes", f.name, f.size);
            }
            h.update(&chunk.bytes);
            out.write_all(&chunk.bytes).await?;
            progress(got);
        }
        if got != f.size {
            bail!("{}: got {got} of {} bytes", f.name, f.size);
        }
        out.sync_all().await?;
        let hash: [u8; 32] = h.finalize().into();
        if hash != f.sha256 {
            bail!("{}: SHA-256 does not match the offer; file discarded", f.name);
        }
        Ok(())
    }
    .await;
    drop(out);
    if let Err(e) = res {
        let _ = recv.stop(0u32.into());
        let _ = tokio::fs::remove_file(&part).await;
        return Err(e);
    }
    tokio::fs::rename(&part, dir.join(&f.name)).await?;
    Ok(())
}
