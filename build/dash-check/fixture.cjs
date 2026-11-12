// Hub API answers for the dashboard check: two machines like the owner's
// (GNOME laptop, Windows PC) with per-machine overrides, so the
// per-machine table is as wide as it gets in practice.
const now = 1794500000;
const settings = {
  emergency_hotkey: "Ctrl+Alt+Shift+Escape",
  clipboard: { enabled: true, text: true, images: true, max_text_bytes: 1048576, max_image_bytes: 16777216 },
  edge: { switch_delay_ms: 0, double_bump: false, double_bump_window_ms: 400, dead_corner_px: 10 },
  path_policy: "prefer_p2p", probe_interval_ms: 1000, log_level: "info",
  allowed_networks: ["10.10.15.0/24"], crossing_key: "Ctrl", cursor_park: "edge", update_policy: "auto",
};
const caps = (capture, inject, clipboard) => ({ capture, inject, clipboard, cursor: { warp: false, hide: false } });
const node = (id, name, os, platform, mons, extra) => Object.assign({
  id, name, hostname: name.toLowerCase(), fingerprint: "ab".repeat(32), os, enabled: true, online: true, status: "online",
  local_addrs: ["10.10.15.23", "fd00::23"], allowed_networks: null, crossing_key: null, cursor_park: null,
  effective_allowed_networks: ["10.10.15.0/24"], effective_crossing_key: "Ctrl", effective_cursor_park: "edge",
  applied_cursor_park: "edge", mouse_speed: 1.0, x: (id - 1) * 1920, y: 0, monitors: mons, caps: caps(true, true, true),
  remote: "10.10.15.23:51234", p2p_addrs: [], agent_version: "0.2.0+106.g1234567890", platform, protocol: 5,
  update_policy: null, effective_update_policy: "auto", offered_version: "0.2.0+106.g1234567890",
  update: { state: "current" }, update_only_reason: null, legacy: null, last_seen: now,
}, extra);
const mon = (x, y, w, h, primary) => ({ name: "m", rect: { x, y, w, h }, primary });
const state = {
  hub: { version: "0.2.0+120.gabcdef0123", fingerprint: "cd".repeat(32), join_token: "FC1-" + "x".repeat(120),
    agent_builds: { version: "0.2.0+120.gabcdef0123", platforms: [{ platform: "linux-x86_64", files: [] }, { platform: "windows-x86_64", files: [] }] } },
  now,
  nodes: [
    node(1, "zorin-laptop", "linux", "linux-x86_64", [mon(0, 0, 1536, 960, true)],
      { allowed_networks: ["10.10.15.0/24", "192.168.100.0/24"], crossing_key: "Ctrl+Alt", cursor_park: "center_hide",
        applied_cursor_park: "edge", effective_cursor_park: "center_hide", update_policy: "notify", effective_update_policy: "notify" }),
    node(2, "windows-desktop-pc", "windows", "windows-x86_64", [mon(0, 0, 2560, 1440, true), mon(2560, 0, 1920, 1080, false)],
      { caps: { capture: true, inject: true, clipboard: true, cursor: { warp: true, hide: true } } }),
  ],
  focus: [], pairs: [{ source: 1, target: 2, path: "p2p", rtt_us: 900, p2p_rtt_us: 900, relay_rtt_us: 1500 }],
  last_clip: null,
};
module.exports = { state, settings };
