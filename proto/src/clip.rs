//! Clipboard payloads.

use crate::NodeId;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClipKind {
    Text,
    Image,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClipContent {
    /// UTF-8 text.
    Text(String),
    /// Image encoded as PNG (the interchange format between all backends).
    Image { png: Vec<u8> },
}

impl ClipContent {
    pub fn kind(&self) -> ClipKind {
        match self {
            ClipContent::Text(_) => ClipKind::Text,
            ClipContent::Image { .. } => ClipKind::Image,
        }
    }

    pub fn len(&self) -> usize {
        match self {
            ClipContent::Text(t) => t.len(),
            ClipContent::Image { png } => png.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Cheap content hash (FNV-1a 64) used to suppress echo loops.
    pub fn hash64(&self) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        let (tag, bytes): (u8, &[u8]) = match self {
            ClipContent::Text(t) => (1, t.as_bytes()),
            ClipContent::Image { png } => (2, png),
        };
        for b in std::iter::once(&tag).chain(bytes.iter()) {
            h ^= *b as u64;
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        h
    }

    /// Whether the clipboard settings allow syncing this content.
    pub fn allowed_by(&self, s: &crate::ClipboardSettings) -> bool {
        if !s.enabled || self.is_empty() {
            return false;
        }
        match self {
            ClipContent::Text(t) => s.text && t.len() <= s.max_text_bytes as usize,
            ClipContent::Image { png } => s.images && png.len() <= s.max_image_bytes as usize,
        }
    }
}

impl std::fmt::Debug for ClipContent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never log clipboard contents.
        match self {
            ClipContent::Text(t) => write!(f, "Text({} bytes)", t.len()),
            ClipContent::Image { png } => write!(f, "Image({} bytes png)", png.len()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClipboardMsg {
    /// Agent → hub: the local clipboard changed. Hub → agent: set your
    /// clipboard (`from` = node where the copy happened).
    Update { from: Option<NodeId>, content: ClipContent },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ClipboardSettings;

    #[test]
    fn clip_limits() {
        let mut s = ClipboardSettings { max_text_bytes: 4, ..Default::default() };
        assert!(ClipContent::Text("abcd".into()).allowed_by(&s));
        assert!(!ClipContent::Text("abcde".into()).allowed_by(&s));
        assert!(!ClipContent::Text(String::new()).allowed_by(&s));
        s.images = false;
        assert!(!ClipContent::Image { png: vec![1] }.allowed_by(&s));
        s.images = true;
        s.enabled = false;
        assert!(!ClipContent::Text("a".into()).allowed_by(&s));
        assert_ne!(ClipContent::Text("a".into()).hash64(), ClipContent::Image { png: b"a".to_vec() }.hash64());
    }

    #[test]
    fn debug_never_prints_content() {
        let s = format!("{:?}", ClipContent::Text("secret password".into()));
        assert!(!s.contains("secret"));
    }
}
