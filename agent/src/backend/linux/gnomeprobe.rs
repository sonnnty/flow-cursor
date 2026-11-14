//! `flow-agent gnome-probe` (hidden): checks the Mutter direct path on a
//! real GNOME session without the hub and without disturbing the user. It
//! creates one session of each kind (see [`super::mutter`]), prints what
//! mutter answers and what the EI sockets offer, and closes everything.
//! Barriers are added and cleared but never enabled, so the pointer is not
//! captured; the RemoteDesktop session shows GNOME's "being controlled"
//! indicator for the second or two it is open.

use super::mutter::{self, call, MutterBus, IC_NAME, IC_PATH, RD_DEVICES, RD_NAME, RD_PATH, RD_SESSION};
use anyhow::{anyhow, Result};
use ashpd::zbus::{self, Connection, Message, OwnedObjectPath, OwnedVariant, Variant};
use flow_layout::{Edge, Side};
use futures_util::StreamExt;
use reis::ei;
use reis::event::{DeviceCapability, EiEvent};
use std::collections::HashMap;
use std::time::Duration;

/// How long the EI sockets are read for seats and devices.
const EI_WATCH: Duration = Duration::from_millis(1500);

/// Runs the probe; true when both sessions worked.
pub async fn run() -> bool {
    let bus = MutterBus::Session;
    let conn = match bus.connect().await {
        Ok(c) => c,
        Err(e) => {
            println!("session bus: {e:#}");
            return false;
        }
    };
    for name in [IC_NAME, RD_NAME, "org.gnome.Mutter.ScreenCast", "org.freedesktop.portal.Desktop"] {
        let on = mutter::on_bus(&conn, name).await.map(|b| if b { "yes" } else { "no" }.to_string());
        println!("{name:<38} {}", on.unwrap_or_else(|e| format!("error: {e:#}")));
    }
    println!();
    let ic = section("InputCapture", capture(&bus, &conn)).await;
    println!();
    let rd = section("RemoteDesktop", remote_desktop(&bus, &conn)).await;
    println!();
    println!("input capture (mutter direct): {}", if ic { "works" } else { "FAILED" });
    println!("remote control (mutter direct): {}", if rd { "works" } else { "FAILED" });
    ic && rd
}

async fn section(title: &str, f: impl std::future::Future<Output = Result<()>>) -> bool {
    println!("== {title}");
    match f.await {
        Ok(()) => true,
        Err(e) => {
            println!("  FAILED: {e:#}");
            false
        }
    }
}

async fn property(conn: &Connection, name: &str, path: &str, iface: &str, prop: &str) -> String {
    let get = async {
        let p = zbus::fdo::PropertiesProxy::builder(conn).destination(name).path(path).build().await?;
        let v: OwnedVariant = p.get(zbus::names::InterfaceName::try_from(iface)?, prop).await?;
        anyhow::Ok(format!("{v:?}"))
    };
    get.await.unwrap_or_else(|e| format!("error: {e:#}"))
}

async fn capture(bus: &MutterBus, conn: &Connection) -> Result<()> {
    println!("  SupportedCapabilities: {}", property(conn, IC_NAME, IC_PATH, IC_NAME, "SupportedCapabilities").await);
    let (_conn, session) = mutter::create_capture_session(bus).await?;
    println!("  CreateSession: {}", session.path());
    let result = async {
        let (serial, mons) = mutter::zones(&session).await?;
        println!("  GetZones: serial {serial}");
        for m in &mons {
            println!("    {} {}x{} at {},{}", m.name, m.rect.w, m.rect.h, m.rect.x, m.rect.y);
        }
        let fd = call("ConnectToEIS", session.call("ConnectToEIS", &())).await?;
        let ctx = mutter::ei_context(fd)?;
        let (_c, events) = ctx
            .handshake_tokio("flowcursor-probe", ei::handshake::ContextType::Receiver)
            .await
            .map_err(|e| anyhow!("EI handshake: {e:?}"))?;
        println!("  ConnectToEIS: receiver handshake done");
        watch_ei(&ctx, events).await;
        // One barrier on an outer edge, cleared again; never enabled.
        let mut added = false;
        for m in &mons {
            for side in [Side::Right, Side::Left, Side::Top, Side::Bottom] {
                let r = m.rect;
                let e = match side {
                    Side::Right => Edge { side, pos: r.x + r.w, start: r.y, end: r.y + r.h, to: flow_proto::NodeId(0) },
                    Side::Left => Edge { side, pos: r.x, start: r.y, end: r.y + r.h, to: flow_proto::NodeId(0) },
                    Side::Top => Edge { side, pos: r.y, start: r.x, end: r.x + r.w, to: flow_proto::NodeId(0) },
                    Side::Bottom => {
                        Edge { side, pos: r.y + r.h, start: r.x, end: r.x + r.w, to: flow_proto::NodeId(0) }
                    }
                };
                let pos = super::wayland::barrier_for(&e).as_tuple();
                match session.call::<_, _, u32>("AddBarrier", &(serial, pos)).await {
                    Ok(id) => {
                        println!("  AddBarrier {pos:?} ({side:?} of {}): id {id}", m.name);
                        added = true;
                        break;
                    }
                    Err(e) => println!("  AddBarrier {pos:?} ({side:?} of {}): refused: {e}", m.name),
                }
            }
            if added {
                break;
            }
        }
        call("ClearBarriers", session.call::<_, _, ()>("ClearBarriers", &())).await?;
        println!("  ClearBarriers: ok (capture was never enabled)");
        if !added {
            return Err(anyhow!("mutter accepted no barrier"));
        }
        Ok(())
    }
    .await;
    match call("Close", session.call::<_, _, ()>("Close", &())).await {
        Ok(()) => println!("  Close: ok"),
        Err(e) => println!("  Close: {e:#}"),
    }
    result
}

async fn remote_desktop(bus: &MutterBus, conn: &Connection) -> Result<()> {
    println!("  Version: {}", property(conn, RD_NAME, RD_PATH, RD_NAME, "Version").await);
    println!("  SupportedDeviceTypes: {}", property(conn, RD_NAME, RD_PATH, RD_NAME, "SupportedDeviceTypes").await);
    let own = bus.connect().await?;
    if !mutter::on_bus(&own, RD_NAME).await? {
        return Err(anyhow!("{RD_NAME} is not on the session bus"));
    }
    let rd = mutter::proxy(&own, RD_NAME, RD_PATH, RD_NAME).await?;
    let path: OwnedObjectPath = call("CreateSession", rd.call("CreateSession", &())).await?;
    println!("  CreateSession: {}", path.as_str());
    let session = mutter::proxy(&own, RD_NAME, path.as_str(), RD_SESSION).await?;
    let mut owner = session.receive_signal("SelectionOwnerChanged").await?;
    let result = async {
        call("Start", session.call::<_, _, ()>("Start", &())).await?;
        println!("  Start: ok");
        let opts: HashMap<&str, Variant> = HashMap::new();
        match call("EnableClipboard", session.call::<_, _, ()>("EnableClipboard", &(opts,))).await {
            Ok(()) => println!("  EnableClipboard: ok"),
            Err(e) => println!("  EnableClipboard: {e:#}"),
        }
        let opts = HashMap::from([("device-types", Variant::from(RD_DEVICES))]);
        let fd = call("ConnectToEIS", session.call("ConnectToEIS", &(opts,))).await?;
        let ctx = mutter::ei_context(fd)?;
        let (_c, events) = ctx
            .handshake_tokio("flowcursor-probe", ei::handshake::ContextType::Sender)
            .await
            .map_err(|e| anyhow!("EI handshake: {e:?}"))?;
        println!("  ConnectToEIS: sender handshake done");
        watch_ei(&ctx, events).await;
        // EnableClipboard announces the current owner right away.
        match tokio::time::timeout(Duration::from_millis(500), owner.next()).await {
            Ok(Some(m)) => print_owner(&m),
            _ => println!("  SelectionOwnerChanged: none (empty clipboard?)"),
        }
        Ok(())
    }
    .await;
    match call("Stop", session.call::<_, _, ()>("Stop", &())).await {
        Ok(()) => println!("  Stop: ok"),
        Err(e) => println!("  Stop: {e:#}"),
    }
    result
}

fn print_owner(m: &Message) {
    match super::portalclip::parse_mutter_owner_changed(m) {
        Ok(c) => println!(
            "  SelectionOwnerChanged: mime types {:?}, session is owner {:?}",
            c.mime_types, c.session_is_owner
        ),
        Err(e) => println!("  SelectionOwnerChanged: unexpected shape: {e:#}"),
    }
}

/// Prints seats (binding everything) and devices with their capabilities
/// and regions for [`EI_WATCH`].
async fn watch_ei(ctx: &ei::Context, mut events: reis::tokio::EiConvertEventStream) {
    let until = tokio::time::sleep(EI_WATCH);
    tokio::pin!(until);
    loop {
        tokio::select! {
            _ = &mut until => break,
            ev = events.next() => match ev {
                Some(Ok(EiEvent::SeatAdded(s))) => {
                    println!("    EI seat {:?}", s.seat.name());
                    s.seat.bind_capabilities(
                        DeviceCapability::Pointer
                            | DeviceCapability::PointerAbsolute
                            | DeviceCapability::Keyboard
                            | DeviceCapability::Scroll
                            | DeviceCapability::Button,
                    );
                    let _ = ctx.flush();
                }
                Some(Ok(EiEvent::DeviceAdded(d))) => {
                    let dev = &d.device;
                    let caps: Vec<&str> = [
                        (DeviceCapability::Pointer, "pointer"),
                        (DeviceCapability::PointerAbsolute, "pointer-absolute"),
                        (DeviceCapability::Keyboard, "keyboard"),
                        (DeviceCapability::Touch, "touch"),
                        (DeviceCapability::Scroll, "scroll"),
                        (DeviceCapability::Button, "button"),
                        (DeviceCapability::Text, "text"),
                    ]
                    .into_iter()
                    .filter(|(c, _)| dev.has_capability(*c))
                    .map(|(_, n)| n)
                    .collect();
                    println!("    EI device {:?} ({:?}): {}", dev.name(), dev.device_type(), caps.join(", "));
                    for r in dev.regions() {
                        println!(
                            "      region {}x{} at {},{} scale {}{}",
                            r.width,
                            r.height,
                            r.x,
                            r.y,
                            r.scale,
                            r.mapping_id.as_deref().map(|m| format!(" mapping {m}")).unwrap_or_default()
                        );
                    }
                }
                Some(Ok(EiEvent::DeviceResumed(r))) => println!("    EI device {:?} resumed", r.device.name()),
                Some(Ok(EiEvent::Disconnected(d))) => {
                    println!("    EI disconnected: {:?}", d.reason);
                    break;
                }
                Some(Ok(_)) => {}
                Some(Err(e)) => {
                    println!("    EI error: {e:?}");
                    break;
                }
                None => {
                    println!("    EI socket closed");
                    break;
                }
            },
        }
    }
}
