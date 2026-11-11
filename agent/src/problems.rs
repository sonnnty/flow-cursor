//! Problems the user should see in the tray / status window (a refused
//! permission dialog, …), keyed so a backend can clear them again when the
//! condition goes away. Process-wide: one agent per process in practice.

use std::collections::BTreeMap;
use std::sync::Mutex;

static PROBLEMS: Mutex<BTreeMap<&'static str, String>> = Mutex::new(BTreeMap::new());

/// Reports (or replaces) the problem `key`.
pub fn set(key: &'static str, message: &str) {
    PROBLEMS.lock().expect("poisoned").insert(key, message.to_string());
}

/// The problem `key` is gone.
pub fn clear(key: &'static str) {
    PROBLEMS.lock().expect("poisoned").remove(key);
}

/// Current problems, ordered by key.
pub fn list() -> Vec<String> {
    PROBLEMS.lock().expect("poisoned").values().cloned().collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn set_replace_clear() {
        super::set("test-a", "one");
        super::set("test-a", "two");
        assert!(super::list().contains(&"two".to_string()));
        assert!(!super::list().contains(&"one".to_string()));
        super::clear("test-a");
        assert!(!super::list().contains(&"two".to_string()));
    }
}
