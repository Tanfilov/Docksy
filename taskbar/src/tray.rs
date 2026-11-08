//! System tray icons. Explorer keeps owning the real tray; our hook DLL (see
//! `trayhook`) copies every `Shell_NotifyIcon` call to us. Parsing and click
//! forwarding are ported from Seelen UI (`hook_dll/src/lib.rs`, `tray_icon.rs`).

use std::rc::Rc;

use windows::Win32::{
    Foundation::{HMODULE, HWND, LPARAM, POINT, WPARAM},
    System::LibraryLoader::{GetProcAddress, LoadLibraryW},
    UI::WindowsAndMessaging::{
        AllowSetForegroundWindow, GetCursorPos, GetWindowThreadProcessId, HHOOK, HICON,
        HWND_BROADCAST, IsWindow, RegisterWindowMessageW, SendNotifyMessageW, SetWindowsHookExW,
        UnhookWindowsHookEx, WH_CALLWNDPROC, WM_CONTEXTMENU, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN,
        WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEMOVE, WM_RBUTTONDOWN, WM_RBUTTONUP,
        WM_USER,
    },
};
use windows::core::{GUID, HSTRING, s, w};

use crate::{icons, util};

pub const FORWARD_TAG: usize = 0x5EE1_7A41;

const NIM_ADD: u32 = 0;
const NIM_MODIFY: u32 = 1;
const NIM_DELETE: u32 = 2;
const NIM_SETVERSION: u32 = 4;
const NIF_MESSAGE: u32 = 0x1;
const NIF_ICON: u32 = 0x2;
const NIF_TIP: u32 = 0x4;
const NIF_STATE: u32 = 0x8;
const NIF_GUID: u32 = 0x20;
const NIS_HIDDEN: u32 = 0x1;
const NIN_SELECT: u32 = WM_USER;
const NIN_POPUPOPEN: u32 = WM_USER + 6;
const NIN_POPUPCLOSE: u32 = WM_USER + 7;

/// NOTIFYICONDATAW as sent to the tray: handles are always 32-bit.
#[repr(C)]
#[derive(Clone, Copy)]
struct NotifyIconData {
    cb_size: u32,
    hwnd: u32,
    uid: u32,
    flags: u32,
    callback_message: u32,
    icon: u32,
    tip: [u16; 128],
    state: u32,
    state_mask: u32,
    info: [u16; 256],
    version: u32,
    info_title: [u16; 64],
    info_flags: u32,
    guid: GUID,
    balloon_icon: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ShellTrayMessage {
    magic: i32,
    message: u32,
    data: NotifyIconData,
}

pub struct TrayIcon {
    pub hwnd: isize,
    pub uid: u32,
    pub guid: Option<GUID>,
    pub callback: u32,
    pub version: u32,
    pub tip: String,
    pub image: Option<Rc<icons::Image>>,
    pub hidden: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayAction {
    Left,
    Double,
    Right,
    Middle,
    Move,
    HoverEnter,
    HoverLeave,
}

#[derive(Default)]
pub struct Tray {
    pub icons: Vec<TrayIcon>,
}

impl Tray {
    fn find(&self, hwnd: isize, uid: u32, guid: Option<GUID>) -> Option<usize> {
        self.icons.iter().position(|i| match (guid, i.guid) {
            (Some(g), Some(ig)) => g == ig,
            _ => i.hwnd == hwnd && i.uid == uid,
        })
    }

    /// Handles a payload forwarded by the hook. Returns true if anything visible changed.
    pub fn handle(&mut self, bytes: &[u8]) -> bool {
        let size = std::mem::size_of::<ShellTrayMessage>();
        let mut buf = vec![0u8; size];
        let n = bytes.len().min(size);
        buf[..n].copy_from_slice(&bytes[..n]);
        let msg: ShellTrayMessage = unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const _) };
        let d = msg.data;

        let hwnd = d.hwnd as i32 as isize;
        let guid = (d.flags & NIF_GUID != 0 && d.guid != GUID::zeroed()).then_some(d.guid);
        let idx = self.find(hwnd, d.uid, guid);

        match msg.message {
            NIM_DELETE => {
                if let Some(i) = idx {
                    self.icons.remove(i);
                    return true;
                }
                false
            }
            NIM_SETVERSION => {
                if let Some(i) = idx {
                    self.icons[i].version = d.version;
                }
                false
            }
            NIM_ADD | NIM_MODIFY => {
                let i = match idx {
                    Some(i) => i,
                    None => {
                        self.icons.push(TrayIcon {
                            hwnd,
                            uid: d.uid,
                            guid,
                            callback: 0,
                            version: 0,
                            tip: String::new(),
                            image: None,
                            hidden: false,
                        });
                        self.icons.len() - 1
                    }
                };
                let icon = &mut self.icons[i];
                if hwnd != 0 {
                    icon.hwnd = hwnd;
                    icon.uid = d.uid;
                }
                if d.flags & NIF_MESSAGE != 0 {
                    icon.callback = d.callback_message;
                }
                if d.flags & NIF_ICON != 0 {
                    let h = HICON(d.icon as i32 as isize as *mut _);
                    if let Some(img) = icons::from_hicon(h) {
                        icon.image = Some(Rc::new(img));
                    }
                }
                if d.flags & NIF_TIP != 0 {
                    icon.tip = util::from_wide(&d.tip).replace('\r', "");
                }
                if d.flags & NIF_STATE != 0 && d.state_mask & NIS_HIDDEN != 0 {
                    icon.hidden = d.state & NIS_HIDDEN != 0;
                }
                true
            }
            _ => false,
        }
    }

    /// Drops icons whose owner window is gone (the app died without removing them).
    pub fn prune(&mut self) -> bool {
        let before = self.icons.len();
        self.icons
            .retain(|i| unsafe { IsWindow(Some(HWND(i.hwnd as *mut _))).as_bool() });
        before != self.icons.len()
    }

    pub fn visible(&self) -> impl Iterator<Item = (usize, &TrayIcon)> {
        self.icons
            .iter()
            .enumerate()
            .filter(|(_, i)| !i.hidden && i.image.is_some())
    }

    pub fn send(&self, idx: usize, action: TrayAction) {
        let Some(icon) = self.icons.get(idx) else { return };
        if icon.callback == 0 {
            return;
        }
        let owner = HWND(icon.hwnd as *mut _);
        if matches!(action, TrayAction::Left | TrayAction::Double | TrayAction::Right | TrayAction::Middle) {
            let mut pid = 0;
            unsafe {
                GetWindowThreadProcessId(owner, Some(&mut pid));
                let _ = AllowSetForegroundWindow(pid);
            }
        }
        let msgs: &[u32] = match action {
            TrayAction::Left => &[WM_LBUTTONDOWN, WM_LBUTTONUP],
            TrayAction::Double => &[WM_LBUTTONDBLCLK, WM_LBUTTONUP],
            TrayAction::Right => &[WM_RBUTTONDOWN, WM_RBUTTONUP],
            TrayAction::Middle => &[WM_MBUTTONDOWN, WM_MBUTTONUP],
            TrayAction::Move => &[WM_MOUSEMOVE],
            TrayAction::HoverEnter | TrayAction::HoverLeave => &[],
        };
        for &m in msgs {
            self.notify(icon, m);
        }
        // explorer sends these for version 3 too
        if icon.version >= 3 {
            let extra = match action {
                TrayAction::Left => Some(NIN_SELECT),
                TrayAction::Right => Some(WM_CONTEXTMENU),
                TrayAction::HoverEnter => Some(NIN_POPUPOPEN),
                TrayAction::HoverLeave => Some(NIN_POPUPCLOSE),
                _ => None,
            };
            if let Some(m) = extra {
                self.notify(icon, m);
            }
        }
    }

    fn notify(&self, icon: &TrayIcon, message: u32) {
        let pack = |lo: u32, hi: u32| (lo & 0xFFFF) | ((hi & 0xFFFF) << 16);
        let (wparam, lparam) = if icon.version > 3 {
            let mut p = POINT::default();
            unsafe {
                let _ = GetCursorPos(&mut p);
            }
            (pack(p.x as u32, p.y as u32), pack(message, icon.uid))
        } else {
            (icon.uid, pack(message, 0))
        };
        unsafe {
            let _ = SendNotifyMessageW(
                HWND(icon.hwnd as *mut _),
                icon.callback,
                WPARAM(wparam as usize),
                LPARAM(lparam as i32 as isize),
            );
        }
    }
}

/// Installed hook into explorer's tray thread.
pub struct TrayHook {
    hook: HHOOK,
    _module: HMODULE,
}

impl Drop for TrayHook {
    fn drop(&mut self) {
        unsafe {
            let _ = UnhookWindowsHookEx(self.hook);
        }
    }
}

pub fn install_hook() -> Result<TrayHook, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let src = exe.with_file_name("seelena_trayhook.dll");
    if !src.exists() {
        return Err(format!("{} not found", src.display()));
    }
    // Explorer keeps the hooked DLL loaded for a while, which would lock the build
    // output. Load a per-run copy instead and clean up copies that are free now.
    let dir = std::path::PathBuf::from(std::env::var("LOCALAPPDATA").unwrap_or_default())
        .join("seelena")
        .join("hook");
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            let _ = std::fs::remove_file(e.path());
        }
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let dll = dir.join(format!("seelena_trayhook_{stamp}.dll"));
    std::fs::copy(&src, &dll).map_err(|e| e.to_string())?;
    unsafe {
        let module = LoadLibraryW(&HSTRING::from(dll.as_os_str())).map_err(|e| e.to_string())?;
        let proc = GetProcAddress(module, s!("CallWndProc")).ok_or("CallWndProc export missing")?;
        let tray = crate::shell::native_taskbars()
            .into_iter()
            .find(|&h| util::window_class(h) == "Shell_TrayWnd")
            .ok_or("Shell_TrayWnd not found")?;
        let tid = GetWindowThreadProcessId(tray, None);
        let hook = SetWindowsHookExW(
            WH_CALLWNDPROC,
            Some(std::mem::transmute::<
                unsafe extern "system" fn() -> isize,
                unsafe extern "system" fn(i32, WPARAM, LPARAM) -> windows::Win32::Foundation::LRESULT,
            >(proc)),
            Some(module.into()),
            tid,
        )
        .map_err(|e| e.to_string())?;
        Ok(TrayHook {
            hook,
            _module: module,
        })
    }
}

pub fn taskbar_created_msg() -> u32 {
    unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) }
}

/// Asks every app to re-add its tray icons (as if explorer restarted).
pub fn broadcast_taskbar_created() {
    unsafe {
        let _ = SendNotifyMessageW(HWND_BROADCAST, taskbar_created_msg(), WPARAM(0), LPARAM(0));
    }
}
