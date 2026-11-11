//! Canonical keycodes for FlowCursor.
//!
//! A key is identified by its USB HID usage, encoded as `page << 16 | id`.
//! Almost everything is on the Keyboard/Keypad page `0x07` (including the
//! modifiers `0xE0..=0xE7`); a few media keys live on the Consumer page `0x0C`.
//!
//! Tables are provided for
//! * Linux evdev `KEY_*` codes (what libei/EIS and evdev use), and
//! * Windows set-1 scancodes as reported by `KBDLLHOOKSTRUCT` /
//!   accepted by `SendInput(KEYEVENTF_SCANCODE)`. Extended (E0-prefixed)
//!   keys are encoded as `0xE000 | code`. Scancodes are layout-independent,
//!   unlike virtual-key codes.

mod names;
mod table;

pub use names::{parse_combo, usage_from_name, usage_name, Combo, ComboError};

/// HID usage page of keyboard keys.
pub const PAGE_KEYBOARD: u32 = 0x07;
/// HID usage page of consumer (media) keys.
pub const PAGE_CONSUMER: u32 = 0x0C;

/// Builds a canonical usage from page and id.
pub const fn usage(page: u32, id: u32) -> u32 {
    (page << 16) | id
}

/// Keyboard-page usage (`0x07`).
pub const fn kb(id: u32) -> u32 {
    usage(PAGE_KEYBOARD, id)
}

/// Commonly referenced usages.
pub mod keys {
    use super::kb;
    pub const A: u32 = kb(0x04);
    pub const ENTER: u32 = kb(0x28);
    pub const ESCAPE: u32 = kb(0x29);
    pub const SPACE: u32 = kb(0x2C);
    pub const CAPS_LOCK: u32 = kb(0x39);
    pub const NUM_LOCK: u32 = kb(0x53);
    pub const PAUSE: u32 = kb(0x48);
    pub const LEFT_CTRL: u32 = kb(0xE0);
    pub const LEFT_SHIFT: u32 = kb(0xE1);
    pub const LEFT_ALT: u32 = kb(0xE2);
    pub const LEFT_META: u32 = kb(0xE3);
    pub const RIGHT_CTRL: u32 = kb(0xE4);
    pub const RIGHT_SHIFT: u32 = kb(0xE5);
    pub const RIGHT_ALT: u32 = kb(0xE6);
    pub const RIGHT_META: u32 = kb(0xE7);
}

/// Whether the usage is one of the eight modifier keys.
pub fn is_modifier(usage: u32) -> bool {
    (keys::LEFT_CTRL..=keys::RIGHT_META).contains(&usage)
}

/// Linux evdev code → canonical usage.
pub fn evdev_to_hid(code: u16) -> Option<u32> {
    table::TABLE.iter().find(|e| e.evdev == code && e.canonical_evdev).map(|e| e.hid)
}

/// Canonical usage → Linux evdev code.
pub fn hid_to_evdev(usage: u32) -> Option<u16> {
    table::TABLE.iter().find(|e| e.hid == usage).map(|e| e.evdev).filter(|c| *c != 0)
}

/// Windows scancode (`0xE0xx` for extended keys) → canonical usage.
pub fn scancode_to_hid(sc: u16) -> Option<u32> {
    table::TABLE.iter().find(|e| e.win == sc && e.win != 0 && e.canonical_win).map(|e| e.hid)
}

/// Canonical usage → Windows scancode (`0xE0xx` for extended keys).
pub fn hid_to_scancode(usage: u32) -> Option<u16> {
    table::TABLE.iter().find(|e| e.hid == usage).map(|e| e.win).filter(|c| *c != 0)
}

/// Builds a Windows scancode from the hook's `scanCode` and extended flag.
pub fn win_scancode(scan: u32, extended: bool) -> u16 {
    let base = (scan & 0xFF) as u16;
    if extended {
        0xE000 | base
    } else {
        base
    }
}

/// Iterates over all known usages (for tests and the dashboard).
pub fn all_usages() -> impl Iterator<Item = u32> {
    table::TABLE.iter().map(|e| e.hid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn evdev_roundtrip_all_canonical_entries() {
        let mut n = 0;
        for e in table::TABLE {
            if e.evdev == 0 {
                continue;
            }
            let back = evdev_to_hid(e.evdev).expect("evdev code maps back");
            if e.canonical_evdev {
                assert_eq!(back, e.hid, "evdev {} round trip", e.evdev);
                assert_eq!(hid_to_evdev(back), Some(e.evdev));
                n += 1;
            } else {
                // Aliases map back to the canonical key with the same evdev code.
                assert_eq!(hid_to_evdev(back), Some(e.evdev));
            }
        }
        assert!(n > 110, "only {n} evdev keys");
    }

    #[test]
    fn scancode_roundtrip_all_canonical_entries() {
        let mut n = 0;
        for e in table::TABLE {
            if e.win == 0 {
                continue;
            }
            let back = scancode_to_hid(e.win).expect("scancode maps back");
            if e.canonical_win {
                assert_eq!(back, e.hid, "scancode {:#x} round trip", e.win);
                n += 1;
            }
            assert_eq!(hid_to_scancode(back), Some(e.win));
        }
        assert!(n > 110, "only {n} scancodes");
    }

    #[test]
    fn hid_to_evdev_to_scancode_chain() {
        // evdev → HID → scancode → HID → evdev is the identity for keys
        // present in both tables (this is the Linux → Windows path).
        for e in table::TABLE.iter().filter(|e| e.evdev != 0 && e.win != 0 && e.canonical_evdev && e.canonical_win) {
            let h = evdev_to_hid(e.evdev).unwrap();
            let sc = hid_to_scancode(h).unwrap();
            let h2 = scancode_to_hid(sc).unwrap();
            assert_eq!(hid_to_evdev(h2), Some(e.evdev));
        }
    }

    #[test]
    fn scancode_to_hid_to_evdev_chain() {
        // scancode → HID → evdev → HID → scancode is the identity for keys
        // present in both tables (this is the Windows → Linux path).
        for e in table::TABLE.iter().filter(|e| e.evdev != 0 && e.win != 0 && e.canonical_evdev && e.canonical_win) {
            let h = scancode_to_hid(e.win).unwrap();
            let ev = hid_to_evdev(h).unwrap();
            let h2 = evdev_to_hid(ev).unwrap();
            assert_eq!(hid_to_scancode(h2), Some(e.win));
        }
    }

    #[test]
    fn extended_scancodes_and_navigation() {
        let nav_keys = [
            (kb(0x49), 110, 0xE052), // Insert
            (kb(0x4A), 102, 0xE047), // Home
            (kb(0x4B), 104, 0xE049), // PageUp
            (kb(0x4C), 111, 0xE053), // Delete
            (kb(0x4D), 107, 0xE04F), // End
            (kb(0x4E), 109, 0xE051), // PageDown
            (kb(0x4F), 106, 0xE04D), // Right
            (kb(0x50), 105, 0xE04B), // Left
            (kb(0x51), 108, 0xE050), // Down
            (kb(0x52), 103, 0xE048), // Up
        ];
        for (usage, evdev, scancode) in nav_keys {
            assert_eq!(evdev_to_hid(evdev), Some(usage));
            assert_eq!(hid_to_evdev(usage), Some(evdev));
            assert_eq!(scancode_to_hid(scancode), Some(usage));
            assert_eq!(hid_to_scancode(usage), Some(scancode));
            // Hook representation: base + extended=true must yield the scancode.
            assert_eq!(win_scancode(scancode as u32 & 0xFF, true), scancode);
            // With extended=false it must NOT match the extended scancode.
            assert_ne!(win_scancode(scancode as u32 & 0xFF, false), scancode);
        }
    }

    #[test]
    fn all_modifiers_roundtrip() {
        let mods = [
            (keys::LEFT_CTRL, 29, 0x001D),
            (keys::LEFT_SHIFT, 42, 0x002A),
            (keys::LEFT_ALT, 56, 0x0038),
            (keys::LEFT_META, 125, 0xE05B),
            (keys::RIGHT_CTRL, 97, 0xE01D),
            (keys::RIGHT_SHIFT, 54, 0x0036),
            (keys::RIGHT_ALT, 100, 0xE038),
            (keys::RIGHT_META, 126, 0xE05C),
        ];
        for (usage, evdev, scancode) in mods {
            assert!(is_modifier(usage));
            assert_eq!(evdev_to_hid(evdev), Some(usage));
            assert_eq!(hid_to_evdev(usage), Some(evdev));
            assert_eq!(scancode_to_hid(scancode), Some(usage));
            assert_eq!(hid_to_scancode(usage), Some(scancode));
        }
    }

    #[test]
    fn media_keys_roundtrip() {
        let media = [
            (usage(PAGE_CONSUMER, 0xCD), 164, 0xE022),  // PlayPause
            (usage(PAGE_CONSUMER, 0xB5), 163, 0xE019),  // NextTrack
            (usage(PAGE_CONSUMER, 0xB6), 165, 0xE010),  // PrevTrack
            (usage(PAGE_CONSUMER, 0xB7), 166, 0xE024),  // Stop
            (usage(PAGE_CONSUMER, 0x223), 172, 0xE032), // BrowserHome
            (usage(PAGE_CONSUMER, 0x224), 158, 0xE06A), // BrowserBack
            (usage(PAGE_CONSUMER, 0x225), 159, 0xE069), // BrowserForward
            (usage(PAGE_CONSUMER, 0x221), 217, 0xE065), // BrowserSearch
            (usage(PAGE_CONSUMER, 0x18A), 155, 0xE06C), // Mail
            (usage(PAGE_CONSUMER, 0x192), 140, 0xE021), // Calculator
        ];
        for (usage, evdev, scancode) in media {
            assert_eq!(evdev_to_hid(evdev), Some(usage));
            assert_eq!(hid_to_evdev(usage), Some(evdev));
            assert_eq!(scancode_to_hid(scancode), Some(usage));
            assert_eq!(hid_to_scancode(usage), Some(scancode));
        }
    }

    #[test]
    fn aliases_map_to_canonical() {
        // NonUsHash (0x07, 0x32) shares evdev 43 and win 0x002B with Backslash (0x07, 0x31).
        let non_us_hash = kb(0x32);
        let backslash = kb(0x31);
        assert_eq!(hid_to_evdev(non_us_hash), Some(43));
        assert_eq!(hid_to_scancode(non_us_hash), Some(0x002B));
        // Reverse lookup must return the canonical usage (Backslash).
        assert_eq!(evdev_to_hid(43), Some(backslash));
        assert_eq!(scancode_to_hid(0x002B), Some(backslash));
    }

    #[test]
    fn edge_case_and_invalid_codes() {
        // Unmapped codes
        assert_eq!(evdev_to_hid(0), None);
        assert_eq!(evdev_to_hid(0xFFFF), None);
        assert_eq!(scancode_to_hid(0), None);
        assert_eq!(scancode_to_hid(0xFFFF), None);
        assert_eq!(hid_to_evdev(0), None);
        assert_eq!(hid_to_scancode(0), None);
        assert_eq!(hid_to_evdev(0xFFFF_FFFF), None);
        assert_eq!(hid_to_scancode(0xFFFF_FFFF), None);

        // win_scancode bit masking
        assert_eq!(win_scancode(0x11E, false), 0x001E);
        assert_eq!(win_scancode(0x11E, true), 0xE01E);
    }

    #[test]
    fn no_duplicate_canonical_codes() {
        let mut hid = HashSet::new();
        let mut ev = HashSet::new();
        let mut win = HashSet::new();
        for e in table::TABLE {
            assert!(hid.insert(e.hid), "duplicate hid {:#x}", e.hid);
            if e.evdev != 0 && e.canonical_evdev {
                assert!(ev.insert(e.evdev), "duplicate evdev {}", e.evdev);
            }
            if e.win != 0 && e.canonical_win {
                assert!(win.insert(e.win), "duplicate scancode {:#x}", e.win);
            }
        }
    }

    #[test]
    fn well_known_keys() {
        // KEY_A = 30, scancode 0x1E
        assert_eq!(evdev_to_hid(30), Some(keys::A));
        assert_eq!(hid_to_scancode(keys::A), Some(0x1E));
        // Right Ctrl is extended: E0 1D, KEY_RIGHTCTRL = 97
        assert_eq!(scancode_to_hid(0xE01D), Some(keys::RIGHT_CTRL));
        assert_eq!(hid_to_evdev(keys::RIGHT_CTRL), Some(97));
        // Keypad Enter vs Enter
        assert_eq!(scancode_to_hid(0x1C), Some(keys::ENTER));
        assert_eq!(scancode_to_hid(0xE01C), Some(kb(0x58)));
        // Arrow keys are extended, keypad digits are not.
        assert_eq!(scancode_to_hid(0xE048), Some(kb(0x52)));
        assert_eq!(scancode_to_hid(0x48), Some(kb(0x60)));
        // Num Lock arrives with the extended flag, Pause without.
        assert_eq!(scancode_to_hid(win_scancode(0x45, true)), Some(keys::NUM_LOCK));
        assert_eq!(scancode_to_hid(win_scancode(0x45, false)), Some(keys::PAUSE));
        // Meta keys
        assert_eq!(evdev_to_hid(125), Some(keys::LEFT_META));
        assert_eq!(scancode_to_hid(0xE05B), Some(keys::LEFT_META));
        // Media keys on the consumer page
        assert_eq!(evdev_to_hid(164), Some(usage(PAGE_CONSUMER, 0xCD)));
        assert_eq!(hid_to_scancode(usage(PAGE_CONSUMER, 0xCD)), Some(0xE022));
        // Unknown codes
        assert_eq!(evdev_to_hid(0), None);
        assert_eq!(evdev_to_hid(0xFFFF), None);
        assert_eq!(scancode_to_hid(0), None);
        assert_eq!(hid_to_evdev(kb(0xFF)), None);
    }

    #[test]
    fn modifiers() {
        assert!(is_modifier(keys::LEFT_CTRL));
        assert!(is_modifier(keys::RIGHT_META));
        assert!(!is_modifier(keys::A));
    }
}
