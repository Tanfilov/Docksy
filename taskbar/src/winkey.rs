//! Win key opens Docksy's Start panel instead of Windows' Start.
//!
//! A low-level keyboard hook watches the Win keys. When Win is pressed and released
//! on its own, a harmless dummy key (VK 0xFF, as PowerToys does) is injected before
//! the release reaches Windows: Windows then treats it as a Win+key combination and
//! doesn't open its Start menu. Combinations like Win+E or Win+X pass through
//! untouched. Keys of elevated (admin) windows don't reach a normal-privilege hook,
//! so over those Windows' own Start still opens.

use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};

use windows::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    System::LibraryLoader::GetModuleHandleW,
    UI::{
        Input::KeyboardAndMouse::{
            INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput,
            VIRTUAL_KEY, VK_LWIN, VK_RWIN,
        },
        WindowsAndMessaging::{
            CallNextHookEx, HHOOK, KBDLLHOOKSTRUCT, PostMessageW, SetWindowsHookExW, UnhookWindowsHookEx,
            WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
        },
    },
};

/// Marks our own injected dummy key, so the hook ignores it.
const DUMMY_TAG: usize = 0x5EE1_0001;
const VK_DUMMY: VIRTUAL_KEY = VIRTUAL_KEY(0xFF);

static ENABLED: AtomicBool = AtomicBool::new(false);
static WIN_DOWN: AtomicBool = AtomicBool::new(false);
/// another key was pressed while Win was held (Win+E etc.)
static WIN_COMBO: AtomicBool = AtomicBool::new(false);
static NOTIFY_HWND: AtomicIsize = AtomicIsize::new(0);
static NOTIFY_MSG: AtomicU32 = AtomicU32::new(0);
static HOOK: AtomicIsize = AtomicIsize::new(0);

fn send_dummy_key() {
    let key = |flags: KEYBD_EVENT_FLAGS| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VK_DUMMY,
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: DUMMY_TAG,
            },
        },
    };
    let inputs = [key(KEYBD_EVENT_FLAGS(0)), key(KEYEVENTF_KEYUP)];
    unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
}

unsafe extern "system" fn hook_proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if code >= 0 && ENABLED.load(Ordering::Relaxed) {
            if let Some(k) = (lp.0 as *const KBDLLHOOKSTRUCT).as_ref() {
                if k.dwExtraInfo != DUMMY_TAG {
                    let msg = wp.0 as u32;
                    let down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
                    let up = msg == WM_KEYUP || msg == WM_SYSKEYUP;
                    let vk = VIRTUAL_KEY(k.vkCode as u16);
                    if vk == VK_LWIN || vk == VK_RWIN {
                        if down && !WIN_DOWN.swap(true, Ordering::Relaxed) {
                            WIN_COMBO.store(false, Ordering::Relaxed);
                        } else if up && WIN_DOWN.swap(false, Ordering::Relaxed) && !WIN_COMBO.load(Ordering::Relaxed) {
                            // lone Win press: cancel Windows' Start, open ours
                            send_dummy_key();
                            let target = HWND(NOTIFY_HWND.load(Ordering::Relaxed) as *mut _);
                            let _ = PostMessageW(Some(target), NOTIFY_MSG.load(Ordering::Relaxed), WPARAM(0), LPARAM(0));
                        }
                    } else if down && WIN_DOWN.load(Ordering::Relaxed) {
                        WIN_COMBO.store(true, Ordering::Relaxed);
                    }
                }
            }
        }
        CallNextHookEx(None, code, wp, lp)
    }
}

/// Installs the hook once; `msg` is posted to `notify` on a lone Win press.
pub fn install(notify: HWND, msg: u32) {
    NOTIFY_HWND.store(notify.0 as isize, Ordering::Relaxed);
    NOTIFY_MSG.store(msg, Ordering::Relaxed);
    if HOOK.load(Ordering::Relaxed) != 0 {
        return;
    }
    let hinst = unsafe { GetModuleHandleW(None) }.unwrap_or_default();
    match unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), Some(hinst.into()), 0) } {
        Ok(h) => HOOK.store(h.0 as isize, Ordering::Relaxed),
        Err(e) => crate::log!("keyboard hook failed: {e}"),
    }
}

pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

pub fn uninstall() {
    let h = HOOK.swap(0, Ordering::Relaxed);
    if h != 0 {
        unsafe {
            let _ = UnhookWindowsHookEx(HHOOK(h as *mut _));
        }
    }
}
