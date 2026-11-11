//! Writes the app icon files into `deploy/icons/` (or the given directory):
//!   cargo run -p flow-tray --example gen-icons [-- DIR]
//! The PNGs of the tray states are for the README / review only.

use flow_tray::icon::{ico, png, Variant};
use std::path::PathBuf;

fn main() -> std::io::Result<()> {
    let dir = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| "deploy/icons".into()));
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("flowcursor.ico"), ico(Variant::App))?;
    for s in [16, 22, 24, 32, 48, 64, 128, 256, 512] {
        std::fs::write(dir.join(format!("flowcursor-{s}.png")), png(Variant::App, s))?;
    }
    let states = dir.join("states");
    std::fs::create_dir_all(&states)?;
    for v in Variant::ALL {
        std::fs::write(states.join(format!("{}-64.png", v.name())), png(v, 64))?;
    }
    println!("icons written to {}", dir.display());
    Ok(())
}
