//! Native taskbar hide/restore and AppBar (reserved screen edge) registration.
//! Hide/restore logic follows Seelen UI's `src/service/shutdown.rs`.

use windows::Win32::{
    Foundation::{HWND, LPARAM, RECT},
    UI::{
        Shell::{
            ABE_BOTTOM, ABE_TOP, ABM_GETSTATE, ABM_NEW, ABM_QUERYPOS, ABM_REMOVE, ABM_SETPOS,
            ABM_SETSTATE, ABS_ALWAYSONTOP, ABS_AUTOHIDE, APPBARDATA, SHAppBarMessage,
        },
        WindowsAndMessaging::{EnumWindows, SW_HIDE, SW_SHOWNOACTIVATE, ShowWindow, ShowWindowAsync},
    },
};
use windows::core::BOOL;

use crate::{config::Position, util};

pub const TASKBAR_CLASSES: [&str; 2] = ["Shell_TrayWnd", "Shell_SecondaryTrayWnd"];

pub fn native_taskbars() -> Vec<HWND> {
    let mut v: Vec<HWND> = Vec::new();
    unsafe extern "system" fn cb(hwnd: HWND, lp: LPARAM) -> BOOL {
        let v = unsafe { &mut *(lp.0 as *mut Vec<HWND>) };
        if TASKBAR_CLASSES.contains(&util::window_class(hwnd).as_str()) {
            v.push(hwnd);
        }
        BOOL(1)
    }
    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(&mut v as *mut _ as isize));
    }
    v
}

fn abd(hwnd: HWND) -> APPBARDATA {
    APPBARDATA {
        cbSize: std::mem::size_of::<APPBARDATA>() as u32,
        hWnd: hwnd,
        ..Default::default()
    }
}

/// File that exists while the native taskbar is hidden by us; holds its original
/// auto-hide state so a crash can be repaired on the next start or with `--restore`.
fn state_file() -> std::path::PathBuf {
    util::data_dir().join("native_taskbar_state")
}

pub fn hide_native() {
    let bars = native_taskbars();
    if !state_file().exists() {
        let state = bars
            .first()
            .map(|&h| unsafe { SHAppBarMessage(ABM_GETSTATE, &mut abd(h)) })
            .unwrap_or(ABS_ALWAYSONTOP as usize);
        let _ = std::fs::write(state_file(), state.to_string());
    }
    for &h in &bars {
        let mut d = abd(h);
        d.lParam = LPARAM(ABS_AUTOHIDE as isize);
        unsafe {
            SHAppBarMessage(ABM_SETSTATE, &mut d);
            let _ = ShowWindow(h, SW_HIDE);
        }
    }
}

/// Hide again after explorer re-shows a taskbar window.
pub fn rehide(hwnd: HWND) {
    unsafe {
        let _ = ShowWindowAsync(hwnd, SW_HIDE);
    }
}

pub fn restore_native() {
    let Ok(saved) = std::fs::read_to_string(state_file()) else {
        return;
    };
    let state: isize = saved.trim().parse().unwrap_or(ABS_ALWAYSONTOP as isize);
    for h in native_taskbars() {
        let mut d = abd(h);
        d.lParam = LPARAM(state);
        unsafe {
            SHAppBarMessage(ABM_SETSTATE, &mut d);
            let _ = ShowWindow(h, SW_SHOWNOACTIVATE);
        }
    }
    let _ = std::fs::remove_file(state_file());
}

pub fn appbar_register(hwnd: HWND, callback_msg: u32) {
    let mut d = abd(hwnd);
    d.uCallbackMessage = callback_msg;
    unsafe { SHAppBarMessage(ABM_NEW, &mut d) };
}

pub fn appbar_remove(hwnd: HWND) {
    unsafe { SHAppBarMessage(ABM_REMOVE, &mut abd(hwnd)) };
}

/// Reserves `thickness` px at the edge of `monitor` and returns the granted rect.
/// Skips ABM_SETPOS when the granted rect equals `current`, so our bars don't
/// ping-pong ABN_POSCHANGED notifications.
pub fn appbar_set_pos(
    hwnd: HWND,
    pos: Position,
    monitor: RECT,
    thickness: i32,
    current: Option<RECT>,
) -> RECT {
    let mut d = abd(hwnd);
    d.uEdge = if pos == Position::Top { ABE_TOP } else { ABE_BOTTOM };
    d.rc = monitor;
    match pos {
        Position::Top => d.rc.bottom = d.rc.top + thickness,
        Position::Bottom => d.rc.top = d.rc.bottom - thickness,
    }
    unsafe { SHAppBarMessage(ABM_QUERYPOS, &mut d) };
    match pos {
        Position::Top => d.rc.bottom = d.rc.top + thickness,
        Position::Bottom => d.rc.top = d.rc.bottom - thickness,
    }
    if current == Some(d.rc) {
        return d.rc;
    }
    unsafe { SHAppBarMessage(ABM_SETPOS, &mut d) };
    d.rc
}
