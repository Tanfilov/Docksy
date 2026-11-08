//! Tray hook DLL, loaded into explorer.exe's `Shell_TrayWnd` thread through
//! `SetWindowsHookExW(WH_CALLWNDPROC)`.
//!
//! Ported from Seelen UI's `src/hook_dll`. The hook is passive: explorer still
//! processes every message. We only copy `Shell_NotifyIcon` payloads (sent to
//! `Shell_TrayWnd` as `WM_COPYDATA` with `dwData == 1`) to the taskbar process,
//! which owns a hidden window of class `SeelenaTrayReceiver`.
//!
//! This code runs inside explorer.exe, so it must never panic or block long.

use std::ffi::c_void;

use windows::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    System::DataExchange::COPYDATASTRUCT,
    UI::WindowsAndMessaging::{
        CWPSTRUCT, CallNextHookEx, FindWindowW, GetClassNameW, SMTO_ABORTIFHUNG, SMTO_BLOCK,
        SendMessageTimeoutW, WM_COPYDATA,
    },
};
use windows::core::{PCWSTR, w};

/// `dwData` tag used when forwarding a tray payload to the taskbar process.
pub const FORWARD_TAG: usize = 0x5EE1_7A41;

/// Smallest tray payload we forward: magic + message + the fixed NOTIFYICONDATAW head
/// (cbSize, hWnd, uID, uFlags, uCallbackMessage, hIcon). Older, shorter structures
/// (V1/V2) are padded with zeros by the receiver.
const MIN_TRAY_PAYLOAD: u32 = 8 + 24;

fn is_tray_window(hwnd: HWND) -> bool {
    const TRAY: &[u16] = &[
        b'S' as u16, b'h' as u16, b'e' as u16, b'l' as u16, b'l' as u16, b'_' as u16, b'T' as u16,
        b'r' as u16, b'a' as u16, b'y' as u16, b'W' as u16, b'n' as u16, b'd' as u16,
    ];
    let mut buf = [0u16; 64];
    let len = unsafe { GetClassNameW(hwnd, &mut buf) };
    len > 0 && &buf[..len as usize] == TRAY
}

unsafe fn forward(data: *const c_void, len: u32) {
    let Ok(target) = (unsafe { FindWindowW(w!("SeelenaTrayReceiver"), PCWSTR::null()) }) else {
        return;
    };
    if target.is_invalid() {
        return;
    }
    let cds = COPYDATASTRUCT {
        dwData: FORWARD_TAG,
        cbData: len,
        lpData: data as *mut c_void,
    };
    let mut result = 0usize;
    unsafe {
        SendMessageTimeoutW(
            target,
            WM_COPYDATA,
            WPARAM(0),
            LPARAM(&cds as *const _ as isize),
            SMTO_ABORTIFHUNG | SMTO_BLOCK,
            250,
            Some(&mut result),
        );
    }
}

unsafe fn handle(lparam: LPARAM) {
    let Some(msg) = (unsafe { (lparam.0 as *const CWPSTRUCT).as_ref() }) else {
        return;
    };
    if msg.message != WM_COPYDATA || !is_tray_window(msg.hwnd) {
        return;
    }
    let Some(cds) = (unsafe { (msg.lParam.0 as *const COPYDATASTRUCT).as_ref() }) else {
        return;
    };
    // Type 1 = Shell_NotifyIcon. Any process can send WM_COPYDATA here, so check the size.
    if cds.dwData != 1 || cds.lpData.is_null() || cds.cbData < MIN_TRAY_PAYLOAD {
        return;
    }
    unsafe { forward(cds.lpData, cds.cbData) };
}

/// # Safety
/// Called by Windows as a `WH_CALLWNDPROC` hook procedure.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn CallWndProc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        // Never let a panic unwind into explorer.
        let _ = std::panic::catch_unwind(|| unsafe { handle(lparam) });
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}
