//! Tray hook DLL, loaded into explorer.exe's `Shell_TrayWnd` thread through
//! `SetWindowsHookExW(WH_CALLWNDPROC)`.
//!
//! Ported from Seelen UI's `src/hook_dll`. Two jobs:
//!
//! 1. Passive: copy `Shell_NotifyIcon` payloads (sent to `Shell_TrayWnd` as
//!    `WM_COPYDATA` with `dwData == 1`) to the taskbar process, which owns a hidden
//!    window of class `DocksyTrayReceiver`. Explorer still processes them.
//! 2. Active: answer `Shell_NotifyIconGetRect` (`WM_COPYDATA`, `dwData == 3`) with
//!    the icon's real position on Docksy's bar. Explorer would answer with the
//!    position in its hidden taskbar, so apps (e.g. Bluetooth) opened their menus in
//!    the wrong place. A hook can't change a reply, so `Shell_TrayWnd` is subclassed;
//!    the reply format follows ManagedShell (dwMessage 1 = top-left, 2 = bottom-right).
//!
//! This code runs inside explorer.exe, so it must never panic or block long. The DLL
//! pins itself before subclassing, so the subclass procedure can never be unloaded
//! while explorer still calls it; without Docksy running it simply passes through.

use std::{
    ffi::c_void,
    sync::atomic::{AtomicIsize, Ordering},
};

use windows::Win32::{
    Foundation::{HANDLE, HMODULE, HWND, LPARAM, LRESULT, WPARAM},
    System::{
        DataExchange::COPYDATASTRUCT,
        LibraryLoader::{
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_PIN,
            GetModuleHandleExW,
        },
    },
    UI::WindowsAndMessaging::{
        CWPSTRUCT, CallNextHookEx, CallWindowProcW, FindWindowW, GWLP_WNDPROC, GetClassNameW,
        GetPropW, SMTO_ABORTIFHUNG, SMTO_BLOCK, SendMessageTimeoutW, SetPropW, SetWindowLongPtrW,
        WM_COPYDATA, WNDPROC,
    },
};
use windows::core::{PCWSTR, w};

/// `dwData` tag used when forwarding a tray payload to the taskbar process.
pub const FORWARD_TAG: usize = 0x5EE1_7A41;
/// `dwData` tag used when asking the taskbar process where an icon is.
pub const RECT_TAG: usize = 0x5EE1_7A42;

/// Smallest tray payload we forward: magic + message + the fixed NOTIFYICONDATAW head
/// (cbSize, hWnd, uID, uFlags, uCallbackMessage, hIcon). Older, shorter structures
/// (V1/V2) are padded with zeros by the receiver.
const MIN_TRAY_PAYLOAD: u32 = 8 + 24;
/// NOTIFYICONIDENTIFIER message: magic, message, cbSize, padding, hWnd, uID, guid.
const ICON_IDENTIFIER_SIZE: u32 = 4 * 6 + 16;

/// Explorer's original `Shell_TrayWnd` window procedure (0 = not subclassed by us).
static ORIGINAL_PROC: AtomicIsize = AtomicIsize::new(0);

fn is_tray_window(hwnd: HWND) -> bool {
    const TRAY: &[u16] = &[
        b'S' as u16, b'h' as u16, b'e' as u16, b'l' as u16, b'l' as u16, b'_' as u16, b'T' as u16,
        b'r' as u16, b'a' as u16, b'y' as u16, b'W' as u16, b'n' as u16, b'd' as u16,
    ];
    let mut buf = [0u16; 64];
    let len = unsafe { GetClassNameW(hwnd, &mut buf) };
    len > 0 && &buf[..len as usize] == TRAY
}

/// Sends a copy of `data` to the taskbar process; returns its reply (0 if unreachable).
unsafe fn send_to_taskbar(tag: usize, data: *const c_void, len: u32, timeout_ms: u32) -> usize {
    let Ok(target) = (unsafe { FindWindowW(w!("DocksyTrayReceiver"), PCWSTR::null()) }) else {
        return 0;
    };
    if target.is_invalid() {
        return 0;
    }
    let cds = COPYDATASTRUCT {
        dwData: tag,
        cbData: len,
        lpData: data as *mut c_void,
    };
    let mut result = 0usize;
    let ok = unsafe {
        SendMessageTimeoutW(
            target,
            WM_COPYDATA,
            WPARAM(0),
            LPARAM(&cds as *const _ as isize),
            SMTO_ABORTIFHUNG | SMTO_BLOCK,
            timeout_ms,
            Some(&mut result),
        )
    };
    if ok.0 == 0 { 0 } else { result }
}

/// Replaces `Shell_TrayWnd`'s window procedure once (per explorer process: a window
/// property marks it, so DLL copies loaded by later Docksy runs don't stack).
unsafe fn ensure_subclassed(tray: HWND) {
    if ORIGINAL_PROC.load(Ordering::Acquire) != 0 {
        return;
    }
    unsafe {
        if !GetPropW(tray, w!("DocksySubclassed")).is_invalid() {
            return;
        }
        // Pin this DLL so it is never unloaded while explorer can still call us.
        let mut module = HMODULE::default();
        if GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_PIN,
            PCWSTR(tray_proc as *const () as *const u16),
            &mut module,
        )
        .is_err()
        {
            return;
        }
        let previous = SetWindowLongPtrW(tray, GWLP_WNDPROC, tray_proc as *const () as isize);
        if previous == 0 {
            return;
        }
        ORIGINAL_PROC.store(previous, Ordering::Release);
        let _ = SetPropW(tray, w!("DocksySubclassed"), Some(HANDLE(1 as *mut c_void)));
    }
}

unsafe fn icon_rect_reply(lparam: LPARAM) -> Option<LRESULT> {
    let cds = unsafe { (lparam.0 as *const COPYDATASTRUCT).as_ref() }?;
    if cds.dwData != 3 || cds.lpData.is_null() || cds.cbData < ICON_IDENTIFIER_SIZE {
        return None;
    }
    let reply = unsafe { send_to_taskbar(RECT_TAG, cds.lpData, cds.cbData, 150) };
    (reply != 0).then_some(LRESULT(reply as isize))
}

unsafe extern "system" fn tray_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_COPYDATA {
        if let Ok(Some(reply)) = std::panic::catch_unwind(|| unsafe { icon_rect_reply(lparam) }) {
            return reply;
        }
    }
    let original = ORIGINAL_PROC.load(Ordering::Acquire);
    let original: WNDPROC = unsafe { std::mem::transmute::<isize, WNDPROC>(original) };
    unsafe { CallWindowProcW(original, hwnd, msg, wparam, lparam) }
}

unsafe fn handle(lparam: LPARAM) {
    let Some(msg) = (unsafe { (lparam.0 as *const CWPSTRUCT).as_ref() }) else {
        return;
    };
    if msg.message != WM_COPYDATA || !is_tray_window(msg.hwnd) {
        return;
    }
    unsafe { ensure_subclassed(msg.hwnd) };
    let Some(cds) = (unsafe { (msg.lParam.0 as *const COPYDATASTRUCT).as_ref() }) else {
        return;
    };
    // Type 1 = Shell_NotifyIcon. Any process can send WM_COPYDATA here, so check the size.
    if cds.dwData != 1 || cds.lpData.is_null() || cds.cbData < MIN_TRAY_PAYLOAD {
        return;
    }
    unsafe { send_to_taskbar(FORWARD_TAG, cds.lpData, cds.cbData, 250) };
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
