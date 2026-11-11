//! Win32 helpers for the Windows tray: opening files/URLs, the hidden
//! window that owns the message loop (and lets `flow-tray --quit` and the
//! installer find the running tray), the small-icon size.

use std::io;
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, FindWindowW, GetMessageW, GetSystemMetrics,
    PostMessageW, PostQuitMessage, RegisterClassW, TranslateMessage, MSG, SM_CXSMICON, SW_SHOWNORMAL, WM_APP, WM_CLOSE,
    WM_DESTROY, WNDCLASSW, WS_POPUP,
};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Opens `target` (URL or file) with its default application.
pub fn shell_open(target: &str) -> io::Result<()> {
    let (verb, file) = (wide("open"), wide(target));
    // SAFETY: valid NUL-terminated strings for the duration of the call.
    let r = unsafe { ShellExecuteW(null_mut(), verb.as_ptr(), file.as_ptr(), null(), null(), SW_SHOWNORMAL) };
    // ShellExecute returns a value > 32 on success.
    if r as usize > 32 {
        Ok(())
    } else {
        Err(io::Error::other(format!("ShellExecute failed for {target} ({})", r as usize)))
    }
}

/// Size of the notification-area icon at the current DPI (16 at 100 %).
pub fn small_icon_size() -> u32 {
    // SAFETY: plain query.
    let s = unsafe { GetSystemMetrics(SM_CXSMICON) };
    (s as u32).clamp(16, 64)
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_CLOSE => {
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

/// A never-shown top-level window of class `class`: `FindWindow` finds it
/// (unlike a message-only window) and WM_CLOSE ends the message loop.
pub fn create_hidden_window(class: &str) -> io::Result<HWND> {
    let class_w = wide(class);
    let title = wide("FlowCursor");
    // SAFETY: the class name outlives registration (static for the
    // process in practice: the window lives until exit).
    unsafe {
        let hinst = GetModuleHandleW(null());
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinst,
            lpszClassName: class_w.as_ptr(),
            ..std::mem::zeroed()
        };
        if RegisterClassW(&wc) == 0 {
            return Err(io::Error::last_os_error());
        }
        let hwnd = CreateWindowExW(
            0,
            class_w.as_ptr(),
            title.as_ptr(),
            WS_POPUP,
            0,
            0,
            0,
            0,
            null_mut(),
            null_mut(),
            hinst,
            null(),
        );
        if hwnd.is_null() {
            return Err(io::Error::last_os_error());
        }
        std::mem::forget(class_w);
        Ok(hwnd)
    }
}

/// Wakes the message loop of the window (`hwnd` as an integer: it is
/// passed between threads).
pub fn wake(hwnd: isize) {
    // SAFETY: posting to a window handle; harmless if it is gone.
    unsafe { PostMessageW(hwnd as HWND, WM_APP, 0, 0) };
}

/// Processes one message; false once the loop should end (WM_QUIT).
pub fn pump_one() -> bool {
    // SAFETY: standard message loop on this thread.
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        if GetMessageW(&mut msg, null_mut(), 0, 0) <= 0 {
            return false;
        }
        TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
    true
}

/// Asks the window of `class` to close; false when there is none.
pub fn close_window(class: &str) -> bool {
    let class_w = wide(class);
    // SAFETY: plain lookups and a posted message.
    unsafe {
        let hwnd = FindWindowW(class_w.as_ptr(), null());
        if hwnd.is_null() {
            return false;
        }
        PostMessageW(hwnd, WM_CLOSE, 0, 0);
        for _ in 0..50 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            if FindWindowW(class_w.as_ptr(), null()).is_null() {
                break;
            }
        }
    }
    true
}
