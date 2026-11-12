//! Windows backend (Windows 10/11, per-user process).
//!
//! * Capture: `WH_MOUSE_LL` / `WH_KEYBOARD_LL` hooks on a dedicated thread
//!   with a message loop. While this machine drives another one, the hooks
//!   swallow local input; relative motion comes from Raw Input (`WM_INPUT`,
//!   `RIDEV_INPUTSINK`). Injected events (ours or from other software) are
//!   never captured. Keys already held when capture starts get their
//!   release passed through so nothing sticks locally.
//! * Edge detection: the mouse hook sees the *unclipped* target position,
//!   so pushing past a screen edge that leads to another machine is
//!   detected even though the cursor stays on the edge pixel.
//! * Crossing key: the keyboard hook tracks the crossing combo's keys
//!   (physical, not injected) and reports them as `LocalKey`; with a
//!   crossing key configured, the mouse hook / raw input path does not even
//!   report an edge hit unless the combo is held, so a game that clips the
//!   cursor (`ClipCursor`) and reads raw input is left completely alone.
//! * Local cursor while driving another machine (cursor-park setting):
//!   moved with `SetCursorPos`. Hidden with a 64x64, almost fully
//!   transparent (alpha 1/255) topmost tool window of ours placed under
//!   the parked cursor, which answers `WM_SETCURSOR` with no cursor. The
//!   hooks swallow physical motion while capturing, so the cursor stays
//!   over that window. Nothing system-wide changes (no `SetSystemCursor`):
//!   when the window is hidden, or the process exits or crashes, the
//!   window under the cursor sets its own cursor again, so no app or game
//!   can be left with a hidden or wrong cursor.
//! * Injection: `SendInput` with **scancodes** (layout independent),
//!   absolute pointer moves over the virtual desktop.
//! * Clipboard: separate thread with `AddClipboardFormatListener`; text
//!   (CF_UNICODETEXT) and images ("PNG" registered format, CF_DIB/CF_DIBV5
//!   converted to PNG).
//!
//! Limits: a normal user process cannot see or inject into the secure
//! desktop (UAC prompts, lock screen, Ctrl+Alt+Del) and, because of UIPI,
//! cannot control elevated windows unless the agent itself runs elevated.

use super::dib;
use crate::backend::{crossing_key_event, Backend, BackendOptions, EventRx, EventTx, LocalEvent};
use crate::park::Park;
use anyhow::{anyhow, Result};
use flow_layout::{Edge, Side};
use flow_net::link::Action;
use flow_proto::{Button, Capabilities, ClipContent, CursorSupport, Monitor, Rect};
use std::cell::{Cell, RefCell};
use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use windows_sys::Win32::Foundation::{GlobalFree, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO};
use windows_sys::Win32::System::DataExchange::{
    AddClipboardFormatListener, CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber,
    IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE};
use windows_sys::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::Input::{
    GetRawInputData, RegisterRawInputDevices, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER, RIDEV_INPUTSINK,
    RID_INPUT, RIM_TYPEMOUSE,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const CF_UNICODETEXT: u32 = 13;
const CF_DIB: u32 = 8;
const CF_DIBV5: u32 = 17;
const WM_APP_WAKE: u32 = WM_APP + 1;
/// To the input window: park the cursor at `unpack(lparam)`, hide it when
/// `wparam` is 1.
const WM_APP_PARK: u32 = WM_APP + 2;
/// To the input window: show the cursor; warp to `unpack(lparam)` when
/// `wparam` is 1.
const WM_APP_UNPARK: u32 = WM_APP + 3;
/// Side of the window that hides the cursor.
const CLOAK_SIZE: i32 = 64;
const MOUSE_MOVE_ABSOLUTE: u16 = 1;

/// State shared between the backend handle and the hook thread.
struct Shared {
    capturing: AtomicBool,
    edges: Mutex<Vec<Edge>>,
    monitors: Mutex<Vec<Monitor>>,
    clip_cmds: Mutex<VecDeque<ClipContent>>,
    clip_hwnd: Mutex<usize>,
    /// Clipboard sequence number after our own last write.
    own_clip_seq: AtomicU32,
    /// Combo that must be held for an edge hit to count (None = any hit).
    crossing: Mutex<Option<flow_keymap::Combo>>,
    /// The input thread's window (cursor park requests go there).
    input_hwnd: AtomicUsize,
    /// `park_cursor` ran since the last `end_capture`.
    parked: AtomicBool,
}

pub struct WindowsBackend {
    shared: Arc<Shared>,
}

/// Per-thread context for the hook callbacks (they carry no user data).
struct HookCtx {
    tx: EventTx,
    shared: Arc<Shared>,
    /// Scancodes physically held (not injected) — updated while not capturing.
    held: HashSet<u16>,
    /// Keys held when capture started: their releases pass through.
    held_at_capture: HashSet<u16>,
    was_capturing: bool,
    /// Crossing-combo keys physically held (usages), whether capturing or not.
    combo_held: HashSet<u32>,
}

impl HookCtx {
    /// The crossing key is configured and not held: edge hits are not
    /// reported at all.
    fn edge_blocked(&self) -> bool {
        let combo = self.shared.crossing.lock().ok().and_then(|c| c.clone());
        !crate::cross::key_held(combo.as_ref(), &self.combo_held)
    }
}

thread_local! {
    static CTX: RefCell<Option<HookCtx>> = const { RefCell::new(None) };
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub async fn create(_opts: &BackendOptions) -> Result<(Arc<dyn Backend>, EventRx)> {
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let (tx, rx) = mpsc::unbounded_channel();
    let shared = Arc::new(Shared {
        capturing: AtomicBool::new(false),
        edges: Mutex::new(Vec::new()),
        monitors: Mutex::new(enum_monitors()),
        clip_cmds: Mutex::new(VecDeque::new()),
        clip_hwnd: Mutex::new(0),
        own_clip_seq: AtomicU32::new(0),
        crossing: Mutex::new(None),
        input_hwnd: AtomicUsize::new(0),
        parked: AtomicBool::new(false),
    });
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
    {
        let (tx, shared) = (tx.clone(), shared.clone());
        std::thread::Builder::new().name("win-input".into()).spawn(move || input_thread(tx, shared, ready_tx))?;
    }
    ready_rx.recv().map_err(|_| anyhow!("input thread died"))?.map_err(|e| anyhow!(e))?;
    {
        let (tx, shared) = (tx.clone(), shared.clone());
        std::thread::Builder::new().name("win-clipboard".into()).spawn(move || clipboard_thread(tx, shared))?;
    }
    info!("Windows backend ready (low-level hooks, raw input, SendInput)");
    Ok((Arc::new(WindowsBackend { shared }), rx))
}

// ---------------------------------------------------------------------------
// monitors
// ---------------------------------------------------------------------------

unsafe extern "system" fn monitor_cb(h: HMONITOR, _dc: HDC, _r: *mut RECT, data: LPARAM) -> windows_sys::core::BOOL {
    let out = &mut *(data as *mut Vec<Monitor>);
    let mut mi: MONITORINFO = std::mem::zeroed();
    mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    if GetMonitorInfoW(h, &mut mi) != 0 {
        let r = mi.rcMonitor;
        out.push(Monitor {
            name: format!("DISPLAY{}", out.len() + 1),
            rect: Rect::new(r.left, r.top, r.right - r.left, r.bottom - r.top),
            primary: mi.dwFlags & MONITORINFOF_PRIMARY != 0,
        });
    }
    1
}

/// Timer of the input window that reads the monitors again after a
/// display change.
const DISPLAY_RECHECK_TIMER: usize = 0x464c; // "FL"
const DISPLAY_RECHECK_MS: u32 = 2000;

/// Reads the monitors and reports them when they changed (runs on the
/// input thread).
fn refresh_monitors() {
    let m = enum_monitors();
    if m.is_empty() {
        debug!("no monitors enumerated; keeping the last ones");
        return;
    }
    with_ctx(|ctx| {
        let changed = match ctx.shared.monitors.lock() {
            Ok(mut mm) if *mm != m => {
                *mm = m.clone();
                true
            }
            _ => false,
        };
        if changed {
            let _ = ctx.tx.send(LocalEvent::MonitorsChanged(m));
        }
    });
}

fn enum_monitors() -> Vec<Monitor> {
    let mut v: Vec<Monitor> = Vec::new();
    unsafe {
        EnumDisplayMonitors(std::ptr::null_mut(), std::ptr::null(), Some(monitor_cb), &mut v as *mut _ as LPARAM);
    }
    v
}

// ---------------------------------------------------------------------------
// input thread: hooks + raw input
// ---------------------------------------------------------------------------

fn input_thread(tx: EventTx, shared: Arc<Shared>, ready: std::sync::mpsc::Sender<Result<(), String>>) {
    unsafe {
        let hinst = GetModuleHandleW(std::ptr::null());
        let class = wide("FlowCursorInput");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(input_wndproc),
            hInstance: hinst,
            lpszClassName: class.as_ptr(),
            ..std::mem::zeroed()
        };
        RegisterClassW(&wc);
        // A hidden top-level window (not message-only) so that broadcast
        // messages such as WM_DISPLAYCHANGE arrive.
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            class.as_ptr(),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinst,
            std::ptr::null(),
        );
        if hwnd.is_null() {
            let _ = ready.send(Err("CreateWindowExW failed".into()));
            return;
        }
        shared.input_hwnd.store(hwnd as usize, Ordering::SeqCst);
        let rid = RAWINPUTDEVICE { usUsagePage: 0x01, usUsage: 0x02, dwFlags: RIDEV_INPUTSINK, hwndTarget: hwnd };
        if RegisterRawInputDevices(&rid, 1, std::mem::size_of::<RAWINPUTDEVICE>() as u32) == 0 {
            let _ = ready.send(Err("RegisterRawInputDevices failed".into()));
            return;
        }
        CTX.with(|c| {
            *c.borrow_mut() = Some(HookCtx {
                tx,
                shared,
                held: HashSet::new(),
                held_at_capture: HashSet::new(),
                was_capturing: false,
                combo_held: HashSet::new(),
            })
        });
        let kb = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), hinst, 0);
        let ms = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), hinst, 0);
        if kb.is_null() || ms.is_null() {
            let _ = ready.send(Err("SetWindowsHookExW failed".into()));
            return;
        }
        let _ = ready.send(Ok(()));
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        UnhookWindowsHookEx(kb);
        UnhookWindowsHookEx(ms);
    }
}

fn with_ctx<R>(f: impl FnOnce(&mut HookCtx) -> R) -> Option<R> {
    CTX.with(|c| c.borrow_mut().as_mut().map(f))
}

/// Updates capture bookkeeping on transitions.
fn sync_capture(ctx: &mut HookCtx) -> bool {
    let cap = ctx.shared.capturing.load(Ordering::SeqCst);
    if cap && !ctx.was_capturing {
        ctx.held_at_capture = ctx.held.clone();
    }
    ctx.was_capturing = cap;
    cap
}

unsafe extern "system" fn keyboard_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let k = &*(lparam as *const KBDLLHOOKSTRUCT);
        let injected = k.flags & LLKHF_INJECTED != 0;
        if !injected {
            let up = k.flags & LLKHF_UP != 0;
            let sc = flow_keymap::win_scancode(k.scanCode, k.flags & LLKHF_EXTENDED != 0);
            let usage = match (k.vkCode as u16, sc) {
                // Pause arrives as scancode 0x45 without the extended flag.
                (VK_PAUSE, _) => Some(flow_keymap::keys::PAUSE),
                (VK_NUMLOCK, _) => Some(flow_keymap::keys::NUM_LOCK),
                _ => flow_keymap::scancode_to_hid(sc),
            };
            let swallow = with_ctx(|ctx| {
                let capturing = sync_capture(ctx);
                // Crossing-combo state follows every physical key; the
                // change is reported only when the key reaches the local OS
                // (captured keys arrive as `Key` instead).
                let combo = ctx.shared.crossing.lock().ok().and_then(|c| c.clone());
                let local_key = usage.and_then(|u| crossing_key_event(combo.as_ref(), &mut ctx.combo_held, u, !up));
                if !capturing {
                    if up {
                        ctx.held.remove(&sc);
                    } else {
                        ctx.held.insert(sc);
                    }
                    if let Some(ev) = local_key {
                        let _ = ctx.tx.send(ev);
                    }
                    return false;
                }
                if up && ctx.held_at_capture.remove(&sc) {
                    // Pressed before we took over: let the local OS see the release.
                    ctx.held.remove(&sc);
                    if let Some(ev) = local_key {
                        let _ = ctx.tx.send(ev);
                    }
                    return false;
                }
                if let Some(usage) = usage {
                    let _ = ctx.tx.send(LocalEvent::Key { usage, down: !up });
                }
                true
            })
            .unwrap_or(false);
            if swallow {
                return 1;
            }
        }
        let _ = wparam;
    }
    CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam)
}

fn edge_hit(edges: &[Edge], x: i32, y: i32) -> Option<(Side, i32, i32)> {
    edges.iter().find_map(|e| {
        let (along, beyond, at) = match e.side {
            Side::Right => (y, x >= e.pos, (e.pos - 1, y)),
            Side::Left => (y, x < e.pos, (e.pos, y)),
            Side::Bottom => (x, y >= e.pos, (x, e.pos - 1)),
            Side::Top => (x, y < e.pos, (x, e.pos)),
        };
        (beyond && along >= e.start && along < e.end).then_some((e.side, at.0, at.1))
    })
}

unsafe extern "system" fn mouse_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let m = &*(lparam as *const MSLLHOOKSTRUCT);
        let injected = m.flags & LLMHF_INJECTED != 0;
        if !injected {
            let msg = wparam as u32;
            let swallow = with_ctx(|ctx| {
                let capturing = sync_capture(ctx);
                if !capturing {
                    if msg == WM_MOUSEMOVE && !ctx.edge_blocked() {
                        let edges = ctx.shared.edges.lock().map(|e| e.clone()).unwrap_or_default();
                        if let Some((side, x, y)) = edge_hit(&edges, m.pt.x, m.pt.y) {
                            let _ = ctx.tx.send(LocalEvent::EdgeHit { side, x, y });
                        }
                    }
                    return false;
                }
                let hi = (m.mouseData >> 16) as u16;
                let ev = match msg {
                    WM_MOUSEMOVE => None, // deltas come from raw input
                    WM_LBUTTONDOWN | WM_LBUTTONUP => {
                        Some(LocalEvent::Button { button: Button::Left, down: msg == WM_LBUTTONDOWN })
                    }
                    WM_RBUTTONDOWN | WM_RBUTTONUP => {
                        Some(LocalEvent::Button { button: Button::Right, down: msg == WM_RBUTTONDOWN })
                    }
                    WM_MBUTTONDOWN | WM_MBUTTONUP => {
                        Some(LocalEvent::Button { button: Button::Middle, down: msg == WM_MBUTTONDOWN })
                    }
                    WM_XBUTTONDOWN | WM_XBUTTONUP => {
                        let button = if hi == XBUTTON1 { Button::Back } else { Button::Forward };
                        Some(LocalEvent::Button { button, down: msg == WM_XBUTTONDOWN })
                    }
                    WM_MOUSEWHEEL => Some(LocalEvent::Wheel { dx: 0, dy: hi as i16 as i32 }),
                    WM_MOUSEHWHEEL => Some(LocalEvent::Wheel { dx: hi as i16 as i32, dy: 0 }),
                    _ => None,
                };
                if let Some(ev) = ev {
                    let _ = ctx.tx.send(ev);
                }
                true
            })
            .unwrap_or(false);
            if swallow {
                return 1;
            }
        }
    }
    CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam)
}

unsafe extern "system" fn input_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_INPUT => {
            let mut raw: RAWINPUT = std::mem::zeroed();
            let mut size = std::mem::size_of::<RAWINPUT>() as u32;
            let n = GetRawInputData(
                lparam as HRAWINPUT,
                RID_INPUT,
                &mut raw as *mut _ as *mut _,
                &mut size,
                std::mem::size_of::<RAWINPUTHEADER>() as u32,
            );
            if n != u32::MAX && raw.header.dwType == RIM_TYPEMOUSE {
                let mouse = raw.data.mouse;
                if mouse.usFlags & MOUSE_MOVE_ABSOLUTE == 0 && (mouse.lLastX != 0 || mouse.lLastY != 0) {
                    let (dx, dy) = (mouse.lLastX, mouse.lLastY);
                    with_ctx(|ctx| {
                        if sync_capture(ctx) {
                            let _ = ctx.tx.send(LocalEvent::Motion { dx: dx as f64, dy: dy as f64 });
                        } else if !ctx.edge_blocked() {
                            // The pointer may sit on the edge pixel when the
                            // driver clips; check the push direction too.
                            let mut p = POINT { x: 0, y: 0 };
                            GetCursorPos(&mut p);
                            let (nx, ny) = (p.x + dx.signum(), p.y + dy.signum());
                            let edges = ctx.shared.edges.lock().map(|e| e.clone()).unwrap_or_default();
                            if let Some((side, x, y)) = edge_hit(&edges, nx, ny) {
                                let _ = ctx.tx.send(LocalEvent::EdgeHit { side, x, y });
                            }
                        }
                    });
                }
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_APP_PARK => {
            let (x, y) = unpack(lparam);
            park_here(x, y, wparam == 1);
            0
        }
        WM_APP_UNPARK => {
            unpark_here((wparam == 1).then(|| unpack(lparam)));
            0
        }
        WM_DISPLAYCHANGE => {
            refresh_monitors();
            // Plugging a monitor in sends this while Windows is still
            // arranging the desktop: look once more when it has settled.
            SetTimer(hwnd, DISPLAY_RECHECK_TIMER, DISPLAY_RECHECK_MS, None);
            0
        }
        WM_TIMER if wparam == DISPLAY_RECHECK_TIMER => {
            KillTimer(hwnd, DISPLAY_RECHECK_TIMER);
            refresh_monitors();
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

// ---------------------------------------------------------------------------
// local cursor while capturing (runs on the input thread)
// ---------------------------------------------------------------------------

thread_local! {
    /// The window that hides the cursor (created on first use).
    static CLOAK: Cell<HWND> = const { Cell::new(std::ptr::null_mut()) };
}

/// Packs a point into an LPARAM (64-bit Windows only, like the rest of
/// the agent).
fn pack(x: i32, y: i32) -> LPARAM {
    (((x as u32 as u64) << 32) | y as u32 as u64) as LPARAM
}

fn unpack(l: LPARAM) -> (i32, i32) {
    let v = l as u64;
    ((v >> 32) as u32 as i32, v as u32 as i32)
}

unsafe extern "system" fn cloak_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        // The cursor is over this window: no cursor at all.
        WM_SETCURSOR => {
            SetCursor(std::ptr::null_mut());
            1
        }
        WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe fn cloak_window() -> HWND {
    let existing = CLOAK.with(|c| c.get());
    if !existing.is_null() {
        return existing;
    }
    let hinst = GetModuleHandleW(std::ptr::null());
    let class = wide("FlowCursorCloak");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(cloak_wndproc),
        hInstance: hinst,
        lpszClassName: class.as_ptr(),
        // No class cursor: WM_SETCURSOR decides.
        hCursor: std::ptr::null_mut(),
        ..std::mem::zeroed()
    };
    RegisterClassW(&wc);
    let hwnd = CreateWindowExW(
        WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
        class.as_ptr(),
        class.as_ptr(),
        WS_POPUP,
        0,
        0,
        CLOAK_SIZE,
        CLOAK_SIZE,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        hinst,
        std::ptr::null(),
    );
    if hwnd.is_null() {
        warn!("cannot create the window that hides the cursor; it stays visible");
        return hwnd;
    }
    // Alpha 1, not 0: invisible, but still hit-tested, so it receives
    // WM_SETCURSOR.
    SetLayeredWindowAttributes(hwnd, 0, 1, LWA_ALPHA);
    CLOAK.with(|c| c.set(hwnd));
    hwnd
}

fn park_here(x: i32, y: i32, hide: bool) {
    unsafe {
        if hide {
            let w = cloak_window();
            if !w.is_null() {
                let half = CLOAK_SIZE / 2;
                SetWindowPos(
                    w,
                    HWND_TOPMOST,
                    x - half,
                    y - half,
                    CLOAK_SIZE,
                    CLOAK_SIZE,
                    SWP_NOACTIVATE | SWP_SHOWWINDOW,
                );
            }
        }
        // After showing the window, so the move lands on it and it gets
        // WM_SETCURSOR.
        SetCursorPos(x, y);
        if hide {
            SetCursor(std::ptr::null_mut());
        }
    }
}

fn unpark_here(warp: Option<(i32, i32)>) {
    unsafe {
        let w = CLOAK.with(|c| c.get());
        if !w.is_null() {
            // The window under the cursor gets WM_SETCURSOR and shows its
            // own cursor again.
            ShowWindow(w, SW_HIDE);
        }
        if let Some((x, y)) = warp {
            SetCursorPos(x, y);
        }
    }
}

// ---------------------------------------------------------------------------
// injection
// ---------------------------------------------------------------------------

fn send(inputs: &[INPUT]) {
    unsafe {
        SendInput(inputs.len() as u32, inputs.as_ptr(), std::mem::size_of::<INPUT>() as i32);
    }
}

fn mouse_input(dx: i32, dy: i32, data: u32, flags: u32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi: MOUSEINPUT { dx, dy, mouseData: data, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    }
}

fn key_input(vk: u16, scan: u16, flags: u32) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: vk, wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    }
}

fn move_abs(x: i32, y: i32) {
    unsafe {
        let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let vw = GetSystemMetrics(SM_CXVIRTUALSCREEN).max(2);
        let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN).max(2);
        let nx = (((x - vx) as i64 * 65535) / (vw - 1) as i64) as i32;
        let ny = (((y - vy) as i64 * 65535) / (vh - 1) as i64) as i32;
        send(&[mouse_input(nx, ny, 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK)]);
    }
}

impl WindowsBackend {
    fn inject_key(&self, usage: u32, down: bool) {
        let up = if down { 0 } else { KEYEVENTF_KEYUP };
        if usage == flow_keymap::keys::PAUSE {
            send(&[key_input(VK_PAUSE, 0, up)]);
            return;
        }
        if usage == flow_keymap::keys::NUM_LOCK {
            send(&[key_input(VK_NUMLOCK, 0, up | KEYEVENTF_EXTENDEDKEY)]);
            return;
        }
        let Some(sc) = flow_keymap::hid_to_scancode(usage) else {
            debug!(usage, "no scancode for usage");
            return;
        };
        let ext = if sc & 0xFF00 == 0xE000 { KEYEVENTF_EXTENDEDKEY } else { 0 };
        send(&[key_input(0, sc & 0xFF, KEYEVENTF_SCANCODE | ext | up)]);
    }
}

impl Backend for WindowsBackend {
    fn name(&self) -> &'static str {
        "windows"
    }
    fn caps(&self) -> Capabilities {
        Capabilities { capture: true, inject: true, clipboard: true, cursor: CursorSupport::FULL }
    }
    fn monitors(&self) -> Vec<Monitor> {
        self.shared.monitors.lock().map(|m| m.clone()).unwrap_or_default()
    }
    fn set_edges(&self, edges: Vec<Edge>) {
        if let Ok(mut e) = self.shared.edges.lock() {
            *e = edges;
        }
    }
    fn begin_capture(&self) {
        self.shared.capturing.store(true, Ordering::SeqCst);
    }
    fn end_capture(&self, warp: Option<(i32, i32)>) {
        self.shared.capturing.store(false, Ordering::SeqCst);
        let hwnd = self.shared.input_hwnd.load(Ordering::SeqCst);
        if self.shared.parked.swap(false, Ordering::SeqCst) && hwnd != 0 {
            // Hide the cloak, then warp, in that order on the input thread.
            let (w, l) = match warp {
                Some((x, y)) => (1, pack(x, y)),
                None => (0, 0),
            };
            unsafe {
                PostMessageW(hwnd as HWND, WM_APP_UNPARK, w, l);
            }
        } else if let Some((x, y)) = warp {
            unsafe {
                SetCursorPos(x, y);
            }
        }
    }
    fn park_cursor(&self, park: Park) {
        let hwnd = self.shared.input_hwnd.load(Ordering::SeqCst);
        if hwnd == 0 {
            return;
        }
        self.shared.parked.store(true, Ordering::SeqCst);
        unsafe {
            PostMessageW(hwnd as HWND, WM_APP_PARK, park.hide as WPARAM, pack(park.at.0, park.at.1));
        }
    }
    fn inject(&self, action: &Action) {
        match *action {
            Action::Entered { x, y } | Action::MoveAbs { x, y, .. } => move_abs(x, y),
            Action::Key { usage, down } => self.inject_key(usage, down),
            Action::Button { button, down } => {
                let (flags, data) = match (button, down) {
                    (Button::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
                    (Button::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
                    (Button::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
                    (Button::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
                    (Button::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
                    (Button::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
                    (Button::Back, true) => (MOUSEEVENTF_XDOWN, XBUTTON1 as u32),
                    (Button::Back, false) => (MOUSEEVENTF_XUP, XBUTTON1 as u32),
                    (Button::Forward, true) => (MOUSEEVENTF_XDOWN, XBUTTON2 as u32),
                    (Button::Forward, false) => (MOUSEEVENTF_XUP, XBUTTON2 as u32),
                };
                send(&[mouse_input(0, 0, data, flags)]);
            }
            Action::Wheel { dx, dy } => {
                if dy != 0 {
                    send(&[mouse_input(0, 0, dy as u32, MOUSEEVENTF_WHEEL)]);
                }
                if dx != 0 {
                    send(&[mouse_input(0, 0, dx as u32, MOUSEEVENTF_HWHEEL)]);
                }
            }
            Action::Left => {}
        }
    }
    fn observes_keys(&self) -> bool {
        true
    }
    fn set_crossing_key(&self, combo: Option<flow_keymap::Combo>) {
        if let Ok(mut c) = self.shared.crossing.lock() {
            *c = combo;
        }
    }
    fn set_clipboard(&self, content: &ClipContent) {
        if let Ok(mut q) = self.shared.clip_cmds.lock() {
            q.push_back(content.clone());
        }
        let hwnd = self.shared.clip_hwnd.lock().map(|h| *h).unwrap_or(0);
        if hwnd != 0 {
            unsafe {
                PostMessageW(hwnd as HWND, WM_APP_WAKE, 0, 0);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// clipboard thread
// ---------------------------------------------------------------------------

thread_local! {
    static CLIP: RefCell<Option<(EventTx, Arc<Shared>, u32)>> = const { RefCell::new(None) };
}

fn clipboard_thread(tx: EventTx, shared: Arc<Shared>) {
    unsafe {
        let hinst = GetModuleHandleW(std::ptr::null());
        let class = wide("FlowCursorClipboard");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(clip_wndproc),
            hInstance: hinst,
            lpszClassName: class.as_ptr(),
            ..std::mem::zeroed()
        };
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            class.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            std::ptr::null_mut(),
            hinst,
            std::ptr::null(),
        );
        if hwnd.is_null() {
            warn!("clipboard window could not be created");
            return;
        }
        let png = wide("PNG");
        let png_fmt = RegisterClipboardFormatW(png.as_ptr());
        if let Ok(mut h) = shared.clip_hwnd.lock() {
            *h = hwnd as usize;
        }
        CLIP.with(|c| *c.borrow_mut() = Some((tx, shared, png_fmt)));
        if AddClipboardFormatListener(hwnd) == 0 {
            warn!("AddClipboardFormatListener failed; clipboard changes on this machine are not synced");
        }
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

unsafe fn open_clipboard(hwnd: HWND) -> bool {
    for _ in 0..10 {
        if OpenClipboard(hwnd) != 0 {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(15));
    }
    false
}

unsafe fn global_bytes(h: *mut core::ffi::c_void) -> Option<Vec<u8>> {
    if h.is_null() {
        return None;
    }
    let p = GlobalLock(h) as *const u8;
    if p.is_null() {
        return None;
    }
    let n = GlobalSize(h);
    let v = std::slice::from_raw_parts(p, n).to_vec();
    GlobalUnlock(h);
    Some(v)
}

unsafe fn read_clipboard(hwnd: HWND, png_fmt: u32) -> Option<ClipContent> {
    if !open_clipboard(hwnd) {
        return None;
    }
    let out = (|| {
        if png_fmt != 0 && IsClipboardFormatAvailable(png_fmt) != 0 {
            if let Some(b) = global_bytes(GetClipboardData(png_fmt)) {
                return Some(ClipContent::Image { png: b });
            }
        }
        for fmt in [CF_DIBV5, CF_DIB] {
            if IsClipboardFormatAvailable(fmt) != 0 {
                if let Some(png) = global_bytes(GetClipboardData(fmt)).and_then(|b| dib::dib_to_png(&b)) {
                    return Some(ClipContent::Image { png });
                }
            }
        }
        if IsClipboardFormatAvailable(CF_UNICODETEXT) != 0 {
            let b = global_bytes(GetClipboardData(CF_UNICODETEXT))?;
            let u16s: Vec<u16> =
                b.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).take_while(|c| *c != 0).collect();
            return Some(ClipContent::Text(String::from_utf16_lossy(&u16s)));
        }
        None
    })();
    CloseClipboard();
    out
}

unsafe fn put_global(fmt: u32, data: &[u8]) -> bool {
    let h = GlobalAlloc(GMEM_MOVEABLE, data.len().max(1));
    if h.is_null() {
        return false;
    }
    let p = GlobalLock(h) as *mut u8;
    if p.is_null() {
        GlobalFree(h);
        return false;
    }
    std::ptr::copy_nonoverlapping(data.as_ptr(), p, data.len());
    GlobalUnlock(h);
    // Only a successful SetClipboardData hands the memory to the system;
    // otherwise it is still ours to free.
    if SetClipboardData(fmt, h).is_null() {
        GlobalFree(h);
        return false;
    }
    true
}

unsafe fn write_clipboard(hwnd: HWND, png_fmt: u32, c: &ClipContent) -> bool {
    if !open_clipboard(hwnd) {
        return false;
    }
    EmptyClipboard();
    let ok = match c {
        ClipContent::Text(t) => {
            let mut bytes: Vec<u8> = t.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
            bytes.extend_from_slice(&[0, 0]);
            put_global(CF_UNICODETEXT, &bytes)
        }
        ClipContent::Image { png } => {
            let a = png_fmt != 0 && put_global(png_fmt, png);
            let b = dib::png_to_dib(png).is_some_and(|d| put_global(CF_DIB, &d));
            a || b
        }
    };
    CloseClipboard();
    ok
}

unsafe extern "system" fn clip_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_CLIPBOARDUPDATE => {
            CLIP.with(|c| {
                if let Some((tx, shared, png_fmt)) = c.borrow().as_ref() {
                    if GetClipboardSequenceNumber() == shared.own_clip_seq.load(Ordering::SeqCst) {
                        return; // our own write
                    }
                    if let Some(content) = read_clipboard(hwnd, *png_fmt) {
                        let _ = tx.send(LocalEvent::Clipboard(content));
                    }
                }
            });
            0
        }
        WM_APP_WAKE => {
            CLIP.with(|c| {
                if let Some((_, shared, png_fmt)) = c.borrow().as_ref() {
                    let items: Vec<ClipContent> =
                        shared.clip_cmds.lock().map(|mut q| q.drain(..).collect()).unwrap_or_default();
                    for item in items {
                        if write_clipboard(hwnd, *png_fmt, &item) {
                            shared.own_clip_seq.store(GetClipboardSequenceNumber(), Ordering::SeqCst);
                        } else {
                            warn!("writing the clipboard failed");
                        }
                    }
                }
            });
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Attaches to the console of the parent process (when started from a
/// terminal), so CLI output is visible although the binary uses the GUI
/// subsystem (no console window at login).
pub fn attach_parent_console() -> bool {
    use windows_sys::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
    unsafe { AttachConsole(ATTACH_PARENT_PROCESS) != 0 }
}
