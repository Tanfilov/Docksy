//! Native taskbar hide/restore and AppBar (reserved screen edge) registration.
//! Hide/restore logic follows Seelen UI's `src/service/shutdown.rs`.

use windows::Win32::{
    Foundation::{HWND, LPARAM, RECT},
    UI::{
        Shell::{
            ABE_BOTTOM, ABE_LEFT, ABE_RIGHT, ABE_TOP, ABM_GETSTATE, ABM_NEW, ABM_QUERYPOS, ABM_REMOVE, ABM_SETPOS,
            ABM_SETSTATE, ABS_ALWAYSONTOP, ABS_AUTOHIDE, APPBARDATA, SHAppBarMessage,
        },
        WindowsAndMessaging::{
            FindWindowExW, IsWindowVisible, SW_HIDE, SW_SHOWNOACTIVATE, ShowWindow,
            ShowWindowAsync,
        },
    },
};
use windows::core::{PCWSTR, w};

use crate::{config::Position, util};

pub const TASKBAR_CLASSES: [&str; 2] = ["Shell_TrayWnd", "Shell_SecondaryTrayWnd"];

/// Native taskbar windows. Uses FindWindowEx: on Windows 11 build 28000, EnumWindows
/// does not return `Shell_TrayWnd`.
pub fn native_taskbars() -> Vec<HWND> {
    let mut v: Vec<HWND> = Vec::new();
    for class in [w!("Shell_TrayWnd"), w!("Shell_SecondaryTrayWnd")] {
        let mut after: Option<HWND> = None;
        while let Ok(h) = unsafe { FindWindowExW(None, after, class, PCWSTR::null()) } {
            if h.is_invalid() || v.contains(&h) {
                break;
            }
            v.push(h);
            after = Some(h);
        }
    }
    v
}

/// Hides any native taskbar window that explorer has shown again.
pub fn enforce_hidden() {
    for h in native_taskbars() {
        if unsafe { IsWindowVisible(h) }.as_bool() {
            rehide(h);
        }
    }
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

/// While explorer starts up at sign-in it re-applies its saved taskbar settings, which
/// can turn auto-hide back off after `hide_native` set it: the taskbar window stays
/// hidden but its screen strip is reserved again (maximized windows leave a dead gap).
/// Sets auto-hide again when that happened; returns true if it did.
pub fn enforce_autohide() -> bool {
    let bars = native_taskbars();
    let Some(&first) = bars.first() else { return false };
    let state = unsafe { SHAppBarMessage(ABM_GETSTATE, &mut abd(first)) };
    if state & ABS_AUTOHIDE as usize != 0 {
        return false;
    }
    for &h in &bars {
        let mut d = abd(h);
        d.lParam = LPARAM(ABS_AUTOHIDE as isize);
        unsafe { SHAppBarMessage(ABM_SETSTATE, &mut d) };
    }
    true
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
    d.uEdge = match pos {
        Position::Top => ABE_TOP,
        Position::Bottom => ABE_BOTTOM,
        Position::Left => ABE_LEFT,
        Position::Right => ABE_RIGHT,
    };
    d.rc = monitor;
    let fit = |rc: &mut RECT| match pos {
        Position::Top => rc.bottom = rc.top + thickness,
        Position::Bottom => rc.top = rc.bottom - thickness,
        Position::Left => rc.right = rc.left + thickness,
        Position::Right => rc.left = rc.right - thickness,
    };
    fit(&mut d.rc);
    unsafe { SHAppBarMessage(ABM_QUERYPOS, &mut d) };
    fit(&mut d.rc);
    if current == Some(d.rc) {
        return d.rc;
    }
    unsafe { SHAppBarMessage(ABM_SETPOS, &mut d) };
    d.rc
}
