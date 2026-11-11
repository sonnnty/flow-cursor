//! Human-readable key names and hotkey combos ("Ctrl+Alt+Shift+Escape").

use crate::keys;
use crate::table::TABLE;

/// Name of a usage as used in combos and the dashboard.
pub fn usage_name(usage: u32) -> Option<&'static str> {
    TABLE.iter().find(|e| e.hid == usage).map(|e| e.name)
}

/// Exact (case-insensitive) key name → usage.
pub fn usage_from_name(name: &str) -> Option<u32> {
    let n = name.trim();
    let alias = match n.to_ascii_lowercase().as_str() {
        "esc" => Some("Escape"),
        "return" => Some("Enter"),
        "del" => Some("Delete"),
        "ins" => Some("Insert"),
        "pgup" => Some("PageUp"),
        "pgdn" | "pgdown" => Some("PageDown"),
        "prtsc" | "print" => Some("PrintScreen"),
        "scrlk" => Some("ScrollLock"),
        _ => None,
    };
    let n = alias.unwrap_or(n);
    TABLE.iter().find(|e| e.name.eq_ignore_ascii_case(n)).map(|e| e.hid)
}

#[derive(Debug, PartialEq, Eq)]
pub enum ComboError {
    Empty,
    UnknownKey(String),
}

impl std::fmt::Display for ComboError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ComboError::Empty => write!(f, "empty key combo"),
            ComboError::UnknownKey(k) => write!(f, "unknown key name {k:?}"),
        }
    }
}

impl std::error::Error for ComboError {}

/// A hotkey: every element must be held; an element is satisfied by any
/// of its alternatives (so "Ctrl" matches left or right Ctrl).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Combo {
    pub parts: Vec<Vec<u32>>,
}

impl Combo {
    /// True when every part has at least one held key.
    pub fn is_held(&self, held: impl Fn(u32) -> bool) -> bool {
        !self.parts.is_empty() && self.parts.iter().all(|alts| alts.iter().any(|k| held(*k)))
    }

    /// Whether `usage` belongs to the combo.
    pub fn contains(&self, usage: u32) -> bool {
        self.parts.iter().any(|alts| alts.contains(&usage))
    }
}

/// Parses `"Ctrl+Alt+Shift+Escape"`. Generic modifier names (Ctrl, Shift,
/// Alt, Super/Win/Meta) match both sides; `LeftCtrl` etc. are exact.
pub fn parse_combo(s: &str) -> Result<Combo, ComboError> {
    let mut parts = Vec::new();
    for raw in s.split('+') {
        let name = raw.trim();
        if name.is_empty() {
            continue;
        }
        let alts = match name.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => vec![keys::LEFT_CTRL, keys::RIGHT_CTRL],
            "shift" => vec![keys::LEFT_SHIFT, keys::RIGHT_SHIFT],
            "alt" => vec![keys::LEFT_ALT, keys::RIGHT_ALT],
            "super" | "win" | "meta" | "cmd" => vec![keys::LEFT_META, keys::RIGHT_META],
            _ => vec![usage_from_name(name).ok_or_else(|| ComboError::UnknownKey(name.to_string()))?],
        };
        parts.push(alts);
    }
    if parts.is_empty() {
        return Err(ComboError::Empty);
    }
    Ok(Combo { parts })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kb;

    #[test]
    fn parse_default_hotkey() {
        let c = parse_combo("Ctrl+Alt+Shift+Escape").unwrap();
        assert_eq!(c.parts.len(), 4);
        let held = [keys::RIGHT_CTRL, keys::LEFT_ALT, keys::LEFT_SHIFT, keys::ESCAPE];
        assert!(c.is_held(|k| held.contains(&k)));
        assert!(!c.is_held(|k| held[..3].contains(&k)));
        assert!(c.contains(keys::ESCAPE));
    }

    #[test]
    fn parse_variants() {
        assert_eq!(parse_combo(" esc ").unwrap().parts, vec![vec![keys::ESCAPE]]);
        assert_eq!(parse_combo("LeftCtrl+F12").unwrap().parts, vec![vec![keys::LEFT_CTRL], vec![kb(0x45)]]);
        assert_eq!(parse_combo("win+l").unwrap().parts[1], vec![kb(0x0F)]);
        assert_eq!(parse_combo(""), Err(ComboError::Empty));
        assert!(matches!(parse_combo("Ctrl+Bogus"), Err(ComboError::UnknownKey(_))));
        assert_eq!(parse_combo("+++").unwrap_err(), ComboError::Empty);

        // Aliases resolution
        assert_eq!(usage_from_name("esc"), Some(keys::ESCAPE));
        assert_eq!(usage_from_name("return"), Some(keys::ENTER));
        assert_eq!(usage_from_name("del"), Some(kb(0x4C)));
        assert_eq!(usage_from_name("ins"), Some(kb(0x49)));
        assert_eq!(usage_from_name("pgup"), Some(kb(0x4B)));
        assert_eq!(usage_from_name("pgdn"), Some(kb(0x4E)));
        assert_eq!(usage_from_name("pgdown"), Some(kb(0x4E)));
        assert_eq!(usage_from_name("prtsc"), Some(kb(0x46)));
        assert_eq!(usage_from_name("print"), Some(kb(0x46)));
        assert_eq!(usage_from_name("scrlk"), Some(kb(0x47)));
        assert_eq!(usage_from_name("unknown_key_xyz"), None);

        // Generic modifier aliases in combo
        let combo_cmd = parse_combo("cmd+shift+a").unwrap();
        assert!(combo_cmd.contains(keys::LEFT_META));
        assert!(combo_cmd.contains(keys::RIGHT_META));
        assert!(combo_cmd.contains(keys::LEFT_SHIFT));
        assert!(combo_cmd.contains(keys::RIGHT_SHIFT));
        assert!(combo_cmd.contains(keys::A));

        // ComboError formatting
        assert_eq!(format!("{}", ComboError::Empty), "empty key combo");
        assert!(format!("{}", ComboError::UnknownKey("abc".into())).contains("abc"));
    }

    #[test]
    fn names_roundtrip() {
        for u in crate::all_usages() {
            let n = usage_name(u).unwrap();
            assert_eq!(usage_from_name(n), Some(u), "{n}");
        }
    }
}
