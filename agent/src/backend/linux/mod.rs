//! Linux backends.
//!
//! * [`wayland`]: GNOME ≥ 45 (and other compositors with the portals):
//!   capture with the xdg-desktop-portal **InputCapture** portal (pointer
//!   barriers + libei receiver context), injection with the
//!   **RemoteDesktop** portal (libei sender context, D-Bus `Notify*`
//!   fallback), clipboard with the portal **Clipboard** interface on the
//!   RemoteDesktop session, falling back to the X11 selection through
//!   XWayland.
//! * [`x11`]: Xorg sessions: XInput2 raw motion + grabs for capture, XTEST
//!   for injection, X11 selections for the clipboard. Pure Rust (x11rb),
//!   no special permissions (unlike evdev/uinput, which need the `input`
//!   group or udev rules).

pub mod gnomeprobe;
pub mod mutter;
pub mod portalclip;
pub mod wayland;
pub mod x11;
pub mod x11clip;

use flow_proto::Button;

// linux/input-event-codes.h
pub const BTN_LEFT: u32 = 0x110;
pub const BTN_RIGHT: u32 = 0x111;
pub const BTN_MIDDLE: u32 = 0x112;
pub const BTN_SIDE: u32 = 0x113;
pub const BTN_EXTRA: u32 = 0x114;
pub const BTN_FORWARD: u32 = 0x115;
pub const BTN_BACK: u32 = 0x116;

pub fn button_to_evdev(b: Button) -> u32 {
    match b {
        Button::Left => BTN_LEFT,
        Button::Right => BTN_RIGHT,
        Button::Middle => BTN_MIDDLE,
        Button::Back => BTN_SIDE,
        Button::Forward => BTN_EXTRA,
    }
}

pub fn evdev_to_button(code: u32) -> Option<Button> {
    Some(match code {
        BTN_LEFT => Button::Left,
        BTN_RIGHT => Button::Right,
        BTN_MIDDLE => Button::Middle,
        BTN_SIDE | BTN_BACK => Button::Back,
        BTN_EXTRA | BTN_FORWARD => Button::Forward,
        _ => return None,
    })
}

/// MIME types offered/accepted for text.
pub const TEXT_MIMES: &[&str] = &["text/plain;charset=utf-8", "UTF8_STRING", "text/plain", "STRING", "TEXT"];
pub const PNG_MIME: &str = "image/png";

/// A private session bus for tests that talk D-Bus to fake services.
#[cfg(test)]
pub(crate) mod testbus {
    use ashpd::zbus::Connection;

    /// A `dbus-daemon` of its own; None when it is not installed (tests
    /// then skip).
    pub struct Bus {
        daemon: std::process::Child,
        pub addr: String,
    }

    impl Bus {
        pub fn start() -> Option<Bus> {
            let mut daemon = std::process::Command::new("dbus-daemon")
                .args(["--session", "--nofork", "--print-address"])
                .stdout(std::process::Stdio::piped())
                .spawn()
                .ok()?;
            let mut addr = String::new();
            std::io::BufRead::read_line(&mut std::io::BufReader::new(daemon.stdout.take()?), &mut addr).ok()?;
            Some(Bus { daemon, addr: addr.trim().to_string() })
        }

        pub async fn connect(&self) -> Connection {
            ashpd::zbus::connection::Builder::address(self.addr.as_str()).build().await.unwrap()
        }
    }

    impl Drop for Bus {
        fn drop(&mut self) {
            let _ = self.daemon.kill();
            let _ = self.daemon.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buttons_roundtrip() {
        for b in Button::ALL {
            assert_eq!(evdev_to_button(button_to_evdev(b)), Some(b));
        }
        assert_eq!(evdev_to_button(0), None);
    }
}
