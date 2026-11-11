//! Agent self-update messages, **frozen since protocol 4** (same rules as
//! [`crate::hello`]: struct layouts never change, enums only gain variants
//! at the end, receivers skip frames they cannot decode).
//!
//! * In a full session they travel inside `FromHub::Update` /
//!   `ToHub::Update`; in an update-only session they are the bare frames of
//!   the control stream.
//! * Files are downloaded on a stream the agent opens: first frame
//!   [`crate::StreamKind::Fetch`], then [`FetchRequest`]; the hub answers
//!   [`FetchReply`] and, if `Ok`, exactly `size` raw bytes, then finishes
//!   the stream.
//!
//! Trust: the agent only installs files whose name, size and SHA-256 came
//! in an [`Offer`] over its authenticated control stream to the hub whose
//! certificate it pinned at join; it asks for files by name and the hub
//! serves only the files listed in its own manifest.

use crate::hello::BuildId;
use serde::{Deserialize, Serialize};

/// Longest accepted file name in a release (names are plain file names).
pub const MAX_FILE_NAME: usize = 64;
/// Upper bound for one file of a release.
pub const MAX_FILE_SIZE: u64 = 512 * 1024 * 1024;
/// Upper bound for the number of files in a release.
pub const MAX_FILES: usize = 64;

/// What an agent does when its hub carries another build (dashboard
/// Settings: global default and per-machine override).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum UpdatePolicy {
    /// Download, then install and restart as soon as this machine is idle.
    #[default]
    Auto,
    /// Tell the user (tray: "update available", "Update now").
    Notify,
    /// Nothing automatic; "Update now" (tray, dashboard) still works.
    Off,
}

impl UpdatePolicy {
    pub const ALL: [UpdatePolicy; 3] = [UpdatePolicy::Auto, UpdatePolicy::Notify, UpdatePolicy::Off];

    pub fn as_str(self) -> &'static str {
        match self {
            UpdatePolicy::Auto => "auto",
            UpdatePolicy::Notify => "notify",
            UpdatePolicy::Off => "off",
        }
    }
}

/// Hub → agent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum UpdateFromHub {
    /// The policy and the build the hub carries for this agent's platform;
    /// sent at connect and whenever either changes.
    Offer(Offer),
    /// Someone pressed "Update now" in the dashboard.
    UpdateNow,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Offer {
    pub policy: UpdatePolicy,
    /// None: the hub carries no build for this platform.
    pub release: Option<Release>,
}

/// One build for one platform.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Release {
    pub id: BuildId,
    pub platform: String,
    pub files: Vec<ReleaseFile>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseFile {
    /// Plain file name (no directories), e.g. `flow-agent`, `flow-tray.exe`.
    pub name: String,
    pub size: u64,
    pub sha256: [u8; 32],
}

/// A release file name is a plain name: no path separators, no `..`, no
/// control characters, not empty, at most [`MAX_FILE_NAME`] bytes.
pub fn valid_file_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_FILE_NAME
        && name != "."
        && name != ".."
        && !name.starts_with('.')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

impl Release {
    /// Checks names, sizes and count (both sides check: the hub its
    /// manifest, the agent every offer).
    pub fn validate(&self) -> Result<(), String> {
        if self.files.is_empty() || self.files.len() > MAX_FILES {
            return Err(format!("a release has 1–{MAX_FILES} files"));
        }
        let mut seen = std::collections::HashSet::new();
        for f in &self.files {
            if !valid_file_name(&f.name) {
                return Err(format!("invalid file name {:?}", f.name));
            }
            if !seen.insert(f.name.as_str()) {
                return Err(format!("file {} listed twice", f.name));
            }
            if f.size == 0 || f.size > MAX_FILE_SIZE {
                return Err(format!("file {} has an invalid size {}", f.name, f.size));
            }
        }
        Ok(())
    }

    pub fn file(&self, name: &str) -> Option<&ReleaseFile> {
        self.files.iter().find(|f| f.name == name)
    }
}

/// Agent → hub.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum UpdateToHub {
    /// The agent's update state, sent on connect and on every change.
    Report(UpdateReport),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
pub struct UpdateReport {
    pub phase: UpdatePhase,
    /// The most recent update that failed (kept until one succeeds); the
    /// agent does not retry that build on its own.
    pub failed: Option<FailedUpdate>,
    pub ext: Vec<(String, String)>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum UpdatePhase {
    /// Runs the build the hub carries (or the hub carries none).
    #[default]
    Current,
    /// Another build is available and not being installed (policy
    /// notify/off, or it failed before).
    Available {
        target: BuildId,
    },
    Downloading {
        target: BuildId,
        done: u64,
        total: u64,
    },
    /// Downloaded and verified; waits until this machine is idle (neither
    /// controlling nor controlled, no keys held).
    Ready {
        target: BuildId,
    },
    Installing {
        target: BuildId,
    },
    /// This agent cannot replace itself (installed by a package…).
    Unsupported {
        reason: String,
    },
}

impl UpdatePhase {
    /// Short name for dashboards and logs.
    pub fn name(&self) -> &'static str {
        match self {
            UpdatePhase::Current => "current",
            UpdatePhase::Available { .. } => "available",
            UpdatePhase::Downloading { .. } => "downloading",
            UpdatePhase::Ready { .. } => "ready",
            UpdatePhase::Installing { .. } => "installing",
            UpdatePhase::Unsupported { .. } => "unsupported",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailedUpdate {
    pub target: BuildId,
    pub reason: String,
    /// The previous version was put back.
    pub rolled_back: bool,
    /// Unix time of the failure.
    pub at: u64,
}

/// First frame (after [`crate::StreamKind::Fetch`]) of a download stream.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchRequest {
    pub build: String,
    pub platform: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FetchReply {
    /// `size` raw bytes follow.
    Ok {
        size: u64,
    },
    Refused {
        reason: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{decode, encode};

    #[test]
    fn file_names() {
        for ok in ["flow-agent", "flow-tray.exe", "flowcursor-64.png", "flow-agent.service", "a_b"] {
            assert!(valid_file_name(ok), "{ok}");
        }
        let long = "x".repeat(65);
        for bad in ["", ".", "..", "../x", "a/b", "a\\b", ".hidden", "a b", "c:x", "x\n", long.as_str(), "é"] {
            assert!(!valid_file_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn release_validation() {
        let f = |n: &str, s| ReleaseFile { name: n.into(), size: s, sha256: [0; 32] };
        let mut r = Release { id: BuildId::new("0.2.0", "1"), platform: "linux-x86_64".into(), files: vec![] };
        assert!(r.validate().is_err(), "empty");
        r.files = vec![f("flow-agent", 10), f("flow-tray", 20)];
        assert!(r.validate().is_ok());
        assert_eq!(r.file("flow-tray").unwrap().size, 20);
        r.files.push(f("flow-agent", 1));
        assert!(r.validate().unwrap_err().contains("twice"));
        r.files.pop();
        r.files.push(f("../evil", 1));
        assert!(r.validate().is_err());
        r.files.pop();
        r.files.push(f("zero", 0));
        assert!(r.validate().is_err());
    }

    #[test]
    fn policy_names() {
        for p in UpdatePolicy::ALL {
            assert_eq!(serde_json::to_string(&p).unwrap(), format!("\"{}\"", p.as_str()));
        }
    }

    /// Pins the frozen layouts (see the module docs).
    #[test]
    fn frozen_layouts() {
        assert_eq!(encode(&UpdateFromHub::UpdateNow), [1]);
        let offer = UpdateFromHub::Offer(Offer {
            policy: UpdatePolicy::Notify,
            release: Some(Release {
                id: BuildId::new("1", "b"),
                platform: "p".into(),
                files: vec![ReleaseFile { name: "f".into(), size: 300, sha256: [7; 32] }],
            }),
        });
        let mut want = vec![0, 1, 1, 1, b'1', 1, b'b', 1, b'p', 1, 1, b'f', 0xac, 0x02];
        want.extend([7; 32]);
        assert_eq!(encode(&offer), want);
        assert_eq!(decode::<UpdateFromHub>(&want).unwrap(), offer);
        let report = UpdateToHub::Report(UpdateReport {
            phase: UpdatePhase::Downloading { target: BuildId::new("1", "b"), done: 1, total: 2 },
            failed: None,
            ext: vec![],
        });
        assert_eq!(encode(&report), [0, 2, 1, b'1', 1, b'b', 1, 2, 0, 0]);
        assert_eq!(encode(&FetchReply::Ok { size: 5 }), [0, 5]);
        let req = FetchRequest { build: "b".into(), platform: "p".into(), name: "f".into() };
        assert_eq!(encode(&req), [1, b'b', 1, b'p', 1, b'f']);
        // A frame from a later protocol (unknown variant) is an error the
        // receiver skips, not a panic.
        assert!(decode::<UpdateFromHub>(&[9]).is_err());
    }
}
