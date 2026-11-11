//! Putting verified files in place.
//!
//! An [`Installer`] decides *where* the files of a release go (a fixed
//! table per platform: the agent never takes a path from the network) and
//! whether this installation can be updated at all. [`apply`] then
//! replaces them one by one, each atomically, after copying the current
//! file to `previous/`; [`restore`] puts the copies back.
//!
//! * Linux: `~/.local/bin/flow-agent` and `flow-tray` (what `install.sh`
//!   installs) plus the unit, desktop entries and icons it installed, each
//!   only if it exists and differs. Replacing = write `<dest>.flowcursor-new`
//!   in the same directory, then `rename` over the old file (a running
//!   program keeps its old inode). An agent in a system path (the `.deb`) is
//!   not touched: "installed by package, update with apt/dpkg".
//! * Windows: `flow-agent.exe` and `flow-tray.exe` next to the running
//!   agent. A running exe cannot be overwritten or deleted but can be
//!   renamed: the old one becomes `<name>.old-<random>` (deleted at a later
//!   start), the new one takes its name.

use anyhow::{anyhow, bail, Context, Result};
use flow_proto::update::{Release, ReleaseFile};
use std::path::{Path, PathBuf};

use super::state::{Backup, ServiceKind};

/// One file to install.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileOp {
    /// Name in the release (and in the staging directory).
    pub name: String,
    pub dest: PathBuf,
    pub executable: bool,
    /// Replace `@BINDIR@` with this directory (unit and desktop files).
    pub bindir: Option<PathBuf>,
}

/// What an installer wants done for a release.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub ops: Vec<FileOp>,
    /// How the agent is restarted afterwards.
    pub service: ServiceKind,
    /// Path of the agent executable (started again after a rollback).
    pub agent_exe: PathBuf,
}

/// Where the files of a release go on this machine.
pub trait Installer: Send + Sync {
    /// Ok when this installation can replace itself; Err = why not (shown
    /// in the tray and the dashboard).
    fn check(&self) -> Result<(), String>;
    /// The release files this installer needs (downloaded).
    fn wanted(&self, release: &Release) -> Vec<ReleaseFile>;
    /// Destinations for the staged files.
    fn plan(&self, release: &Release) -> Result<Plan>;
    /// After [`apply`]: platform extras (icon cache, Apps list version).
    fn after_install(&self, _release: &Release) {}
}

/// Content of `name` from the staging directory as it will be installed
/// (`@BINDIR@` filled in for templates).
fn rendered(staged: &Path, op: &FileOp) -> Result<Vec<u8>> {
    let bytes = std::fs::read(staged.join(&op.name)).with_context(|| format!("reading staged {}", op.name))?;
    Ok(match &op.bindir {
        Some(dir) => String::from_utf8(bytes)
            .map_err(|_| anyhow!("{} is not text", op.name))?
            .replace("@BINDIR@", &dir.to_string_lossy())
            .into_bytes(),
        None => bytes,
    })
}

/// Writes `data` next to `dest` and moves it into place.
fn put(dest: &Path, data: &[u8], executable: bool) -> Result<()> {
    use std::io::Write;
    let dir = dest.parent().ok_or_else(|| anyhow!("{} has no directory", dest.display()))?;
    std::fs::create_dir_all(dir)?;
    let file_name = dest.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let tmp = dir.join(format!("{file_name}.flowcursor-new"));
    {
        let mut f = std::fs::File::create(&tmp).with_context(|| format!("writing {}", tmp.display()))?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = if executable { 0o755 } else { 0o644 };
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    let _ = executable;
    let r = replace(&tmp, dest);
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r
}

/// Moves `src` (same directory) over `dest`.
#[cfg(not(windows))]
fn replace(src: &Path, dest: &Path) -> Result<()> {
    std::fs::rename(src, dest).with_context(|| format!("replacing {}", dest.display()))
}

/// Windows: a running exe can be renamed but not replaced: move the old
/// file aside first, then the new one in (and back if that fails).
#[cfg(windows)]
fn replace(src: &Path, dest: &Path) -> Result<()> {
    let aside = if dest.exists() {
        let a = old_name(dest);
        std::fs::rename(dest, &a).with_context(|| format!("moving {} aside", dest.display()))?;
        Some(a)
    } else {
        None
    };
    if let Err(e) = std::fs::rename(src, dest) {
        if let Some(a) = aside {
            let _ = std::fs::rename(&a, dest);
        }
        return Err(e).with_context(|| format!("replacing {}", dest.display()));
    }
    if let Some(a) = aside {
        // Fails while the old program still runs; removed at a later start.
        let _ = std::fs::remove_file(&a);
    }
    Ok(())
}

#[cfg(windows)]
fn old_name(dest: &Path) -> PathBuf {
    let name = dest.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    dest.with_file_name(format!("{name}.old-{:08x}", rand::random::<u32>()))
}

/// Deletes `*.old-*` files a Windows update left next to `exe` (they are
/// in use until the old processes are gone).
pub fn remove_leftovers(exe_dir: &Path) {
    let Ok(rd) = std::fs::read_dir(exe_dir) else { return };
    for e in rd.flatten() {
        let n = e.file_name().to_string_lossy().to_string();
        if n.contains(".old-") || n.ends_with(".flowcursor-new") {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// The ops whose destination would change (identical files are skipped).
pub fn changes(plan: &Plan, staged: &Path) -> Result<Vec<FileOp>> {
    let mut out = Vec::new();
    for op in &plan.ops {
        let new = rendered(staged, op)?;
        if std::fs::read(&op.dest).ok().as_deref() != Some(&new[..]) {
            out.push(op.clone());
        }
    }
    Ok(out)
}

/// Replaces the files of `ops` with the staged ones, after saving the
/// current files in `previous` (emptied first). On error everything done
/// so far is undone. Returns the backups (what [`restore`] needs).
pub fn apply(ops: &[FileOp], staged: &Path, previous: &Path) -> Result<Vec<Backup>> {
    // Check everything is readable before touching anything.
    let contents: Vec<Vec<u8>> = ops.iter().map(|op| rendered(staged, op)).collect::<Result<_>>()?;
    let _ = std::fs::remove_dir_all(previous);
    std::fs::create_dir_all(previous).with_context(|| format!("creating {}", previous.display()))?;
    let mut backups = Vec::new();
    for (i, op) in ops.iter().enumerate() {
        let saved = if op.dest.exists() {
            let to = previous.join(format!("{i:02}-{}", op.name));
            std::fs::copy(&op.dest, &to).with_context(|| format!("saving {}", op.dest.display()))?;
            Some(to)
        } else {
            None
        };
        backups.push(Backup { dest: op.dest.clone(), saved, executable: op.executable });
    }
    for (i, op) in ops.iter().enumerate() {
        if let Err(e) = put(&op.dest, &contents[i], op.executable) {
            let _ = restore(&backups[..i]);
            return Err(e);
        }
    }
    Ok(backups)
}

/// Puts the saved files back (and removes files that did not exist).
/// Tries every file; the first error is returned.
pub fn restore(backups: &[Backup]) -> Result<()> {
    let mut first = None;
    for b in backups {
        let r = match &b.saved {
            Some(saved) => std::fs::read(saved)
                .with_context(|| format!("reading {}", saved.display()))
                .and_then(|data| put(&b.dest, &data, b.executable)),
            None => remove(&b.dest),
        };
        if let Err(e) = r {
            first.get_or_insert(e);
        }
    }
    match first {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

fn remove(p: &Path) -> Result<()> {
    #[cfg(windows)]
    if p.exists() {
        // Maybe running: rename first.
        let a = old_name(p);
        std::fs::rename(p, &a)?;
        let _ = std::fs::remove_file(&a);
        return Ok(());
    }
    match std::fs::remove_file(p) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

// ----------------------------------------------------------------------
// Linux
// ----------------------------------------------------------------------

/// Files of an `install.sh` installation.
#[derive(Clone, Debug)]
pub struct LinuxInstaller {
    /// The running agent executable.
    pub exe: PathBuf,
    pub home: PathBuf,
    pub data_home: PathBuf,
    pub config_home: PathBuf,
    /// Running as the systemd user service.
    pub systemd: bool,
}

impl LinuxInstaller {
    /// For this process (`$HOME`, XDG directories, `INVOCATION_ID`).
    pub fn from_env(exe: PathBuf) -> Option<Self> {
        let home = std::env::var_os("HOME").map(PathBuf::from)?;
        let xdg = |var: &str, default: &str| {
            std::env::var_os(var).map(PathBuf::from).filter(|p| p.is_absolute()).unwrap_or_else(|| home.join(default))
        };
        Some(LinuxInstaller {
            data_home: xdg("XDG_DATA_HOME", ".local/share"),
            config_home: xdg("XDG_CONFIG_HOME", ".config"),
            systemd: std::env::var_os("INVOCATION_ID").is_some(),
            home,
            exe,
        })
    }

    fn bindir(&self) -> PathBuf {
        self.home.join(".local/bin")
    }

    /// Destination of a release file, and whether it is installed even
    /// when missing (binaries) or only replaced when present.
    fn dest(&self, name: &str) -> Option<(PathBuf, bool, bool)> {
        let icons = self.data_home.join("icons/hicolor");
        // (dest, executable, template)
        Some(match name {
            "flow-agent" | "flow-tray" => (self.bindir().join(name), true, false),
            "flow-agent.service" => (self.config_home.join("systemd/user/flow-agent.service"), false, true),
            "flowcursor.desktop" => (self.data_home.join("applications/flowcursor.desktop"), false, true),
            "flowcursor-tray.desktop" => (self.config_home.join("autostart/flowcursor-tray.desktop"), false, true),
            "flowcursor.svg" => (icons.join("scalable/apps/flowcursor.svg"), false, false),
            _ => {
                let size: u32 = name.strip_prefix("flowcursor-")?.strip_suffix(".png")?.parse().ok()?;
                (icons.join(format!("{size}x{size}/apps/flowcursor.png")), false, false)
            }
        })
    }
}

impl Installer for LinuxInstaller {
    fn check(&self) -> Result<(), String> {
        let dir = self.exe.parent().map(Path::to_path_buf).unwrap_or_default();
        if dir == self.bindir() {
            return Ok(());
        }
        if self.exe.starts_with("/usr") || self.exe.starts_with("/opt") {
            return Err(format!(
                "installed by package ({}): update with apt/dpkg (sudo apt install ./flow-agent_<version>_amd64.deb)",
                self.exe.display()
            ));
        }
        Err(format!("not installed by install.sh (running {}): update it by hand", self.exe.display()))
    }

    fn wanted(&self, release: &Release) -> Vec<ReleaseFile> {
        release.files.iter().filter(|f| self.dest(&f.name).is_some()).cloned().collect()
    }

    fn plan(&self, release: &Release) -> Result<Plan> {
        if release.file("flow-agent").is_none() {
            bail!("the release has no flow-agent");
        }
        let mut ops = Vec::new();
        for f in self.wanted(release) {
            let (dest, executable, template) = self.dest(&f.name).expect("wanted");
            // Binaries always; the rest only where install.sh put them and
            // the user kept them (a removed autostart entry stays removed).
            if !executable && !dest.exists() {
                continue;
            }
            ops.push(FileOp { name: f.name, dest, executable, bindir: template.then(|| self.bindir()) });
        }
        Ok(Plan {
            ops,
            service: if self.systemd { ServiceKind::Systemd } else { ServiceKind::Plain },
            agent_exe: self.bindir().join("flow-agent"),
        })
    }

    fn after_install(&self, _release: &Release) {
        let icons = self.data_home.join("icons/hicolor");
        let _ = std::process::Command::new("gtk-update-icon-cache")
            .args(["-q", "-t"])
            .arg(&icons)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

// ----------------------------------------------------------------------
// Windows
// ----------------------------------------------------------------------

/// The exes of a per-user install (setup exe or `install.ps1`).
#[derive(Clone, Debug)]
pub struct WindowsInstaller {
    pub exe: PathBuf,
}

impl WindowsInstaller {
    fn dir(&self) -> PathBuf {
        self.exe.parent().map(Path::to_path_buf).unwrap_or_default()
    }
}

impl Installer for WindowsInstaller {
    fn check(&self) -> Result<(), String> {
        let dir = self.dir();
        if !dir.join("flow-tray.exe").exists() {
            return Err(format!("not a FlowCursor installation ({} has no flow-tray.exe)", dir.display()));
        }
        let probe = dir.join(format!(".flowcursor-write-test-{}", std::process::id()));
        match std::fs::write(&probe, b"") {
            Ok(()) => {
                let _ = std::fs::remove_file(&probe);
                Ok(())
            }
            Err(e) => Err(format!("cannot write to {} ({e}): update with the setup exe", dir.display())),
        }
    }

    fn wanted(&self, release: &Release) -> Vec<ReleaseFile> {
        release
            .files
            .iter()
            .filter(|f| matches!(f.name.as_str(), "flow-agent.exe" | "flow-tray.exe"))
            .cloned()
            .collect()
    }

    fn plan(&self, release: &Release) -> Result<Plan> {
        if release.file("flow-agent.exe").is_none() {
            bail!("the release has no flow-agent.exe");
        }
        let ops = self
            .wanted(release)
            .into_iter()
            .map(|f| FileOp { dest: self.dir().join(&f.name), name: f.name, executable: true, bindir: None })
            .collect();
        Ok(Plan { ops, service: ServiceKind::Plain, agent_exe: self.dir().join("flow-agent.exe") })
    }

    fn after_install(&self, release: &Release) {
        // The version in Settings → Apps (only if the setup exe made the
        // entry; install.ps1 installs have none).
        const KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\FlowCursor";
        let mut q = std::process::Command::new("reg");
        q.args(["query", KEY, "/v", "DisplayVersion"]);
        if hidden(&mut q).status().is_ok_and(|s| s.success()) {
            let mut add = std::process::Command::new("reg");
            add.args(["add", KEY, "/v", "DisplayVersion", "/t", "REG_SZ", "/d", &release.id.version, "/f"]);
            let _ = hidden(&mut add).status();
        }
    }
}

/// No console window for helper programs (Windows).
fn hidden(cmd: &mut std::process::Command) -> &mut std::process::Command {
    cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// The installer for this process (by platform).
pub fn for_this_process(exe: PathBuf) -> std::sync::Arc<dyn Installer> {
    #[cfg(windows)]
    {
        std::sync::Arc::new(WindowsInstaller { exe })
    }
    #[cfg(not(windows))]
    {
        match LinuxInstaller::from_env(exe.clone()) {
            Some(l) => std::sync::Arc::new(l),
            None => std::sync::Arc::new(Unsupported(format!("no $HOME: cannot update {}", exe.display()))),
        }
    }
}

/// An installation that cannot update itself.
pub struct Unsupported(pub String);

impl Installer for Unsupported {
    fn check(&self) -> Result<(), String> {
        Err(self.0.clone())
    }
    fn wanted(&self, _: &Release) -> Vec<ReleaseFile> {
        Vec::new()
    }
    fn plan(&self, _: &Release) -> Result<Plan> {
        bail!("{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flow_proto::BuildId;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("flow-install-{tag}-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn release(names: &[&str]) -> Release {
        Release {
            id: BuildId::new("0.2.0", "2"),
            platform: "linux-x86_64".into(),
            files: names.iter().map(|n| ReleaseFile { name: n.to_string(), size: 1, sha256: [0; 32] }).collect(),
        }
    }

    #[test]
    fn linux_plan_follows_install_sh() {
        let home = tmp("home");
        let li = LinuxInstaller {
            exe: home.join(".local/bin/flow-agent"),
            data_home: home.join(".local/share"),
            config_home: home.join(".config"),
            systemd: true,
            home: home.clone(),
        };
        assert!(li.check().is_ok());
        // Present: unit and one icon; absent: autostart entry (removed by the user).
        std::fs::create_dir_all(home.join(".config/systemd/user")).unwrap();
        std::fs::write(home.join(".config/systemd/user/flow-agent.service"), "old").unwrap();
        std::fs::create_dir_all(home.join(".local/share/icons/hicolor/64x64/apps")).unwrap();
        std::fs::write(home.join(".local/share/icons/hicolor/64x64/apps/flowcursor.png"), "png").unwrap();
        let r = release(&[
            "flow-agent",
            "flow-tray",
            "flow-agent.service",
            "flowcursor-tray.desktop",
            "flowcursor-64.png",
            "flowcursor-x.png",
            "install.sh",
            "FlowCursor-setup-0.2.0.exe",
        ]);
        let names: Vec<String> = li.wanted(&r).into_iter().map(|f| f.name).collect();
        assert_eq!(
            names,
            ["flow-agent", "flow-tray", "flow-agent.service", "flowcursor-tray.desktop", "flowcursor-64.png"]
        );
        let plan = li.plan(&r).unwrap();
        let dests: Vec<_> = plan.ops.iter().map(|o| o.dest.strip_prefix(&home).unwrap().to_path_buf()).collect();
        assert_eq!(
            dests,
            [
                PathBuf::from(".local/bin/flow-agent"),
                ".local/bin/flow-tray".into(),
                ".config/systemd/user/flow-agent.service".into(),
                ".local/share/icons/hicolor/64x64/apps/flowcursor.png".into(),
            ]
        );
        assert_eq!(plan.ops[2].bindir.as_deref(), Some(home.join(".local/bin").as_path()));
        assert_eq!(plan.service, ServiceKind::Systemd);

        let deb = LinuxInstaller { exe: "/usr/bin/flow-agent".into(), ..li.clone() };
        assert!(deb.check().unwrap_err().contains("apt"));
        let dev = LinuxInstaller { exe: "/src/target/debug/flow-agent".into(), ..li };
        assert!(dev.check().unwrap_err().contains("by hand"));
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn apply_and_restore() {
        let root = tmp("apply");
        let staged = root.join("staged");
        let prev = root.join("previous");
        std::fs::create_dir_all(&staged).unwrap();
        std::fs::write(staged.join("flow-agent"), "new agent").unwrap();
        std::fs::write(staged.join("unit"), "ExecStart=@BINDIR@/flow-agent run").unwrap();
        std::fs::write(staged.join("same"), "same").unwrap();
        std::fs::write(staged.join("added"), "added").unwrap();
        let dest = root.join("installed");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("flow-agent"), "old agent").unwrap();
        std::fs::write(dest.join("unit"), "old unit").unwrap();
        std::fs::write(dest.join("same"), "same").unwrap();
        let op = |n: &str, bindir: Option<PathBuf>| FileOp {
            name: n.into(),
            dest: dest.join(n),
            executable: n == "flow-agent",
            bindir,
        };
        let plan = Plan {
            ops: vec![op("flow-agent", None), op("unit", Some("/b".into())), op("same", None), op("added", None)],
            ..Default::default()
        };
        let ops = changes(&plan, &staged).unwrap();
        assert_eq!(ops.iter().map(|o| o.name.as_str()).collect::<Vec<_>>(), ["flow-agent", "unit", "added"]);
        let backups = apply(&ops, &staged, &prev).unwrap();
        assert_eq!(std::fs::read_to_string(dest.join("flow-agent")).unwrap(), "new agent");
        assert_eq!(std::fs::read_to_string(dest.join("unit")).unwrap(), "ExecStart=/b/flow-agent run");
        assert!(dest.join("added").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dest.join("flow-agent")).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o755);
        }
        assert!(backups[2].saved.is_none(), "added file had no previous version");
        restore(&backups).unwrap();
        assert_eq!(std::fs::read_to_string(dest.join("flow-agent")).unwrap(), "old agent");
        assert_eq!(std::fs::read_to_string(dest.join("unit")).unwrap(), "old unit");
        assert!(!dest.join("added").exists());

        // A failure half-way undoes what was done.
        let bad = vec![op("flow-agent", None), op("missing", None)];
        assert!(apply(&bad, &staged, &prev).is_err());
        assert_eq!(std::fs::read_to_string(dest.join("flow-agent")).unwrap(), "old agent");
        std::fs::remove_dir_all(root).ok();
    }
}
