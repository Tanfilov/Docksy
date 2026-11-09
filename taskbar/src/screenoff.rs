//! "Turn off screen" without sleeping.
//!
//! On Modern Standby PCs, any screen-off that Windows knows about (SC_MONITORPOWER,
//! the screen timeout) puts the PC into standby, which pauses desktop apps. So the
//! screens are turned off behind Windows' back instead: every monitor gets a black,
//! cursorless window (an OLED panel's pixels are then off), and monitors that speak
//! DDC/CI are switched to standby. Windows is asked to keep the display "on"
//! meanwhile, so its own timeout can't start standby. Any mouse or keyboard input
//! brings everything back.

use std::{
    cell::RefCell,
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
};

use windows::Win32::{
    Devices::Display::{
        DestroyPhysicalMonitors, GetNumberOfPhysicalMonitorsFromHMONITOR, GetPhysicalMonitorsFromHMONITOR,
        PHYSICAL_MONITOR, SetVCPFeature,
    },
    Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM},
    Graphics::Gdi::{BLACK_BRUSH, EnumDisplayMonitors, GetStockObject, HBRUSH, HDC, HMONITOR},
    System::{
        LibraryLoader::GetModuleHandleW,
        Power::{ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED, SetThreadExecutionState},
        SystemInformation::GetTickCount,
    },
    UI::{
        Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO},
        WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DestroyWindow, HWND_TOPMOST, KillTimer, RegisterClassW,
            SWP_NOACTIVATE, SWP_SHOWWINDOW, SetCursor, SetTimer, SetWindowPos, WM_SETCURSOR, WNDCLASSW,
            WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
        },
    },
};
use windows::core::{BOOL, w};

/// MCCS "power mode" VCP code: 1 = on, 4 = standby (DPM off)
const VCP_POWER: u8 = 0xD6;

static ACTIVE: AtomicBool = AtomicBool::new(false);
static INPUT_AT: AtomicU32 = AtomicU32::new(0);
static REGISTERED: AtomicBool = AtomicBool::new(false);

thread_local! {
    static OVERLAYS: RefCell<Vec<HWND>> = const { RefCell::new(Vec::new()) };
}

/// Turns the screens off until the next mouse or keyboard input.
pub fn start() {
    if ACTIVE.swap(true, Ordering::AcqRel) {
        return;
    }
    crate::log!("screen off");
    unsafe {
        SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED | ES_DISPLAY_REQUIRED);
        // a moment later, so the click's own mouse movement doesn't end it at once
        SetTimer(None, 0, 700, Some(blank));
    }
}

fn last_input() -> u32 {
    let mut li = LASTINPUTINFO { cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
    if unsafe { GetLastInputInfo(&mut li) }.as_bool() { li.dwTime } else { unsafe { GetTickCount() } }
}

unsafe extern "system" fn overlay_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if msg == WM_SETCURSOR {
            SetCursor(None);
            return LRESULT(1);
        }
        DefWindowProcW(hwnd, msg, wp, lp)
    }
}

unsafe extern "system" fn collect_rect(_: HMONITOR, _: HDC, r: *mut RECT, data: LPARAM) -> BOOL {
    unsafe { (*(data.0 as *mut Vec<RECT>)).push(*r) };
    BOOL(1)
}

unsafe extern "system" fn collect_monitor(m: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
    unsafe { (*(data.0 as *mut Vec<isize>)).push(m.0 as isize) };
    BOOL(1)
}

unsafe extern "system" fn blank(_: HWND, _: u32, id: usize, _: u32) {
    unsafe {
        let _ = KillTimer(None, id);
        let hinst = GetModuleHandleW(None).unwrap_or_default();
        if !REGISTERED.swap(true, Ordering::AcqRel) {
            let wc = WNDCLASSW {
                lpfnWndProc: Some(overlay_proc),
                hInstance: hinst.into(),
                hbrBackground: HBRUSH(GetStockObject(BLACK_BRUSH).0),
                lpszClassName: w!("DocksyScreenOff"),
                ..Default::default()
            };
            RegisterClassW(&wc);
        }
        let mut rects: Vec<RECT> = Vec::new();
        let _ = EnumDisplayMonitors(None, None, Some(collect_rect), LPARAM(&mut rects as *mut _ as isize));
        let mut windows = Vec::new();
        for r in rects {
            if let Ok(h) = CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
                w!("DocksyScreenOff"),
                w!("Docksy screen off"),
                WS_POPUP,
                r.left,
                r.top,
                r.right - r.left,
                r.bottom - r.top,
                None,
                None,
                Some(hinst.into()),
                None,
            ) {
                let _ = SetWindowPos(
                    h,
                    Some(HWND_TOPMOST),
                    r.left,
                    r.top,
                    r.right - r.left,
                    r.bottom - r.top,
                    SWP_SHOWWINDOW | SWP_NOACTIVATE,
                );
                windows.push(h);
            }
        }
        OVERLAYS.with(|o| *o.borrow_mut() = windows);
        INPUT_AT.store(last_input(), Ordering::Relaxed);
        std::thread::spawn(|| monitors_power(false));
        SetTimer(None, 0, 200, Some(watch));
    }
}

/// Any input: screens back on.
unsafe extern "system" fn watch(_: HWND, _: u32, id: usize, _: u32) {
    if last_input() == INPUT_AT.load(Ordering::Relaxed) {
        return;
    }
    unsafe {
        let _ = KillTimer(None, id);
        OVERLAYS.with(|o| {
            for h in o.borrow_mut().drain(..) {
                let _ = DestroyWindow(h);
            }
        });
        SetThreadExecutionState(ES_CONTINUOUS);
    }
    std::thread::spawn(|| monitors_power(true));
    ACTIVE.store(false, Ordering::Release);
    crate::log!("screen on");
}

/// DDC/CI power on/standby for every external monitor that supports it (slow:
/// runs on its own thread). Monitors without DDC/CI just stay black.
fn monitors_power(on: bool) {
    let mut monitors: Vec<isize> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(collect_monitor), LPARAM(&mut monitors as *mut _ as isize));
        for m in monitors {
            let m = HMONITOR(m as *mut _);
            let mut n = 0u32;
            if GetNumberOfPhysicalMonitorsFromHMONITOR(m, &mut n).is_err() || n == 0 {
                continue;
            }
            let mut phys = vec![PHYSICAL_MONITOR::default(); n as usize];
            if GetPhysicalMonitorsFromHMONITOR(m, &mut phys).is_err() {
                continue;
            }
            for p in &phys {
                let ok = SetVCPFeature(p.hPhysicalMonitor, VCP_POWER, if on { 1 } else { 4 });
                crate::log!("ddc power {} -> {}", if on { "on" } else { "standby" }, ok != 0);
            }
            let _ = DestroyPhysicalMonitors(&phys);
        }
    }
}
