//! Keeps the tray icon registered with the StatusNotifierWatcher.
//!
//! `ksni` registers the item once and again when the watcher name gets a
//! new owner, and never looks at the watcher's list again. The list can
//! lose the item without any of that happening: GNOME's AppIndicator
//! extension forgets an item when the connection that owned its name
//! vanishes, which is also what happened to a tray restarted in place (same
//! pid, same name, see [`crate::actions::restart_tray`]); the extension can
//! be disabled and enabled, the shell restarted. The process keeps running
//! and owns its name, but no icon is shown.
//!
//! The supervisor therefore reads `RegisteredStatusNotifierItems` every 30 s
//! and shortly after the watcher changed owner. When the list does not
//! contain this process any more, it asks the tray loop to register again:
//! to shut the item down and start a new one, which owns a new name
//! (`org.kde.StatusNotifierItem-<pid>-<n+1>`) on a new connection, so the
//! host cannot confuse it with the one it dropped. Cheap: one blocking
//! D-Bus connection, one property read per check.

use std::sync::mpsc;
use std::time::{Duration, Instant};
use zbus::blocking::fdo::DBusProxy;
use zbus::blocking::Connection;
use zbus::proxy::CacheProperties;

pub const WATCHER: &str = "org.kde.StatusNotifierWatcher";
/// How often the watcher's list is read.
pub const CHECK_EVERY: Duration = Duration::from_secs(30);
/// The first look after the watcher changed owner: `ksni` registers again
/// by itself, give it time.
const AFTER_WATCHER_CHANGE: Duration = Duration::from_secs(5);
/// A host that keeps not listing the item (it may not implement the list)
/// gets fewer and fewer new items: up to this long between attempts.
const MAX_BACKOFF: Duration = Duration::from_secs(600);
/// Overrides [`CHECK_EVERY`] in seconds (tests of the tray).
pub const ENV_CHECK_SECS: &str = "FLOW_TRAY_SNI_CHECK_SECS";

#[zbus::proxy(
    interface = "org.kde.StatusNotifierWatcher",
    default_service = "org.kde.StatusNotifierWatcher",
    default_path = "/StatusNotifierWatcher",
    gen_async = false
)]
trait Watcher {
    #[zbus(property)]
    fn registered_status_notifier_items(&self) -> zbus::Result<Vec<String>>;
}

/// True when the watcher's list has an item of the process `pid`.
///
/// Entries are `<bus name>/<object path>` (GNOME's extension, KDE) or just
/// the name; the item's name is `org.kde.StatusNotifierItem-<pid>-<n>`.
pub fn lists(items: &[String], pid: u32) -> bool {
    let prefix = format!("org.kde.StatusNotifierItem-{pid}-");
    items.iter().any(|i| i.starts_with(&prefix))
}

/// When to look next, and how patient to be with a host that does not list
/// the item.
#[derive(Debug)]
pub struct Schedule {
    base: Duration,
    misses: u32,
}

impl Schedule {
    pub fn new(base: Duration) -> Self {
        Schedule { base, misses: 0 }
    }

    /// Records a check; returns how long to wait for the next one. The
    /// first miss is looked at again after `base`, then the wait doubles up
    /// to [`MAX_BACKOFF`]; a listed item starts over.
    pub fn after_check(&mut self, listed: bool) -> Duration {
        if listed {
            self.misses = 0;
            return self.base;
        }
        self.misses = self.misses.saturating_add(1);
        let doublings = (self.misses - 1).min(16);
        self.base.saturating_mul(1 << doublings).min(MAX_BACKOFF.max(self.base))
    }
}

enum Msg {
    /// The watcher got an owner (it started, or came back).
    WatcherChanged,
}

/// Starts the supervisor thread; `register_again` runs on it whenever the
/// host does not list this process's item. Without a session bus there is
/// nothing to watch.
pub fn supervise(register_again: impl Fn() + Send + 'static) {
    let base = std::env::var(ENV_CHECK_SECS)
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|s| *s > 0.0)
        .map(Duration::from_secs_f64)
        .unwrap_or(CHECK_EVERY);
    let r = std::thread::Builder::new().name("sni-supervisor".into()).spawn(move || {
        if let Err(e) = run(base, register_again) {
            eprintln!("flow-tray: not watching the tray host ({e}); the icon is not re-registered");
        }
    });
    if let Err(e) = r {
        eprintln!("flow-tray: starting the tray host watcher: {e}");
    }
}

fn run(base: Duration, register_again: impl Fn()) -> zbus::Result<()> {
    let conn = Connection::session()?;
    let dbus = DBusProxy::new(&conn)?;
    let (tx, rx) = mpsc::channel();
    let changes = dbus.receive_name_owner_changed_with_args(&[(0, WATCHER)])?;
    std::thread::Builder::new()
        .name("sni-watcher-owner".into())
        .spawn(move || {
            for sig in changes {
                // Only "has an owner now" matters; a vanished watcher
                // leaves nothing to register with.
                if sig.args().is_ok_and(|a| a.new_owner.is_some()) && tx.send(Msg::WatcherChanged).is_err() {
                    return;
                }
            }
        })
        .map_err(|e| zbus::Error::Failure(e.to_string()))?;
    let watcher = WatcherProxy::builder(&conn).cache_properties(CacheProperties::No).build()?;
    let pid = std::process::id();
    let mut schedule = Schedule::new(base);
    let mut due = Instant::now() + base;
    loop {
        match rx.recv_timeout(due.saturating_duration_since(Instant::now())) {
            Ok(Msg::WatcherChanged) => {
                due = due.min(Instant::now() + AFTER_WATCHER_CHANGE);
                continue;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        }
        // A watcher that is not there (no host, shell restarting) or does
        // not answer: nothing to register with, try again later.
        let listed = match watcher.registered_status_notifier_items() {
            Ok(items) => lists(&items, pid),
            Err(_) => true,
        };
        if !listed {
            eprintln!("flow-tray: the tray host does not list the icon; registering it again");
            register_again();
        }
        due = Instant::now() + schedule.after_check(listed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn finds_the_item_of_this_process() {
        let me = 4242;
        assert!(lists(&s(&["org.kde.StatusNotifierItem-4242-1/StatusNotifierItem"]), me), "GNOME extension");
        assert!(lists(&s(&["org.kde.StatusNotifierItem-4242-2"]), me), "name only");
        assert!(lists(&s(&[":1.5/x", "org.kde.StatusNotifierItem-4242-3/StatusNotifierItem"]), me));
        assert!(!lists(&[], me), "dropped");
        assert!(!lists(&s(&["org.kde.StatusNotifierItem-424-1/StatusNotifierItem"]), me), "another pid");
        assert!(!lists(&s(&["org.kde.StatusNotifierItem-42421-1/StatusNotifierItem"]), me), "another pid");
        assert!(!lists(&s(&["org.kde.StatusNotifierItem-1-4242/StatusNotifierItem"]), me), "pid is not the counter");
    }

    #[test]
    fn schedule_backs_off_for_a_host_that_never_lists_the_item() {
        let mut sc = Schedule::new(Duration::from_secs(30));
        assert_eq!(sc.after_check(true), Duration::from_secs(30));
        assert_eq!(sc.after_check(false), Duration::from_secs(30), "first miss: look again soon");
        assert_eq!(sc.after_check(false), Duration::from_secs(60));
        assert_eq!(sc.after_check(false), Duration::from_secs(120));
        for _ in 0..40 {
            assert!(sc.after_check(false) <= MAX_BACKOFF);
        }
        assert_eq!(sc.after_check(false), MAX_BACKOFF);
        assert_eq!(sc.after_check(true), Duration::from_secs(30), "listed again: start over");
        // A test interval longer than the cap is kept.
        let mut long = Schedule::new(Duration::from_secs(900));
        assert_eq!(long.after_check(false), Duration::from_secs(900));
    }
}
