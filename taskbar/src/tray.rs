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
/// Forwarded `Shell_NotifyIconGetRect` request (see trayhook).
pub const RECT_TAG: usize = 0x5EE1_7A42;

/// NOTIFYICONIDENTIFIER as sent to the tray (ManagedShell's WINNOTIFYICONIDENTIFIER).
#[repr(C)]
#[derive(Clone, Copy)]
struct IconIdentifierMessage {
    magic: i32,
    /// 1 = asks for the top-left corner, 2 = for the bottom-right corner
    message: i32,
    cb_size: i32,
    padding: i32,
    hwnd: u32,
    uid: u32,
    guid: GUID,
}

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
    /// "Show on taskbar" in Windows settings; false = lives in the overflow flyout
    pub promoted: bool,
    /// position in Windows' tray order (UIOrderList), unknown = last
    pub order: usize,
    /// owner executable (lowercase), resolved once
    exe: Option<String>,
}

/// Per-icon settings Windows keeps in `HKCU\Control Panel\NotifyIconSettings\<id>`.
struct RegistryIcon {
    key: String,
    exe: String,
    guid: Option<GUID>,
    uid: Option<u32>,
    promoted: bool,
}

const NOTIFY_ICON_SETTINGS: &str = r"Control Panel\NotifyIconSettings";

fn reg_value(subkey: &str, name: &str, flags: windows::Win32::System::Registry::REG_ROUTINE_FLAGS, buf: &mut [u8]) -> Option<usize> {
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RegGetValueW};
    let mut size = buf.len() as u32;
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            &HSTRING::from(subkey),
            &HSTRING::from(name),
            flags,
            None,
            Some(buf.as_mut_ptr() as *mut _),
            Some(&mut size),
        )
        .ok()
        .ok()?;
    }
    Some(size as usize)
}

fn reg_string(subkey: &str, name: &str) -> Option<String> {
    use windows::Win32::System::Registry::RRF_RT_REG_SZ;
    let mut buf = [0u8; 2048];
    let n = reg_value(subkey, name, RRF_RT_REG_SZ, &mut buf)?;
    let wide: Vec<u16> = buf[..n].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    Some(util::from_wide(&wide))
}

fn reg_dword(subkey: &str, name: &str) -> Option<u32> {
    use windows::Win32::System::Registry::RRF_RT_REG_DWORD;
    let mut buf = [0u8; 4];
    reg_value(subkey, name, RRF_RT_REG_DWORD, &mut buf)?;
    Some(u32::from_le_bytes(buf))
}

/// `{6D809377-...}\App\app.exe` -> `C:\Program Files\App\app.exe` (known-folder prefix).
fn resolve_known_folder(path: &str) -> String {
    use windows::Win32::UI::Shell::{KF_FLAG_DEFAULT, SHGetKnownFolderPath};
    if let Some(rest) = path.strip_prefix('{') {
        if let Some(end) = rest.find('}') {
            if let Ok(id) = GUID::try_from(&rest[..end]) {
                if let Ok(p) = unsafe { SHGetKnownFolderPath(&id, KF_FLAG_DEFAULT, None) } {
                    let base = unsafe { p.to_string() }.unwrap_or_default();
                    unsafe { windows::Win32::System::Com::CoTaskMemFree(Some(p.0 as *const _)) };
                    return format!("{base}{}", &rest[end + 1..]);
                }
            }
        }
    }
    path.to_string()
}

fn read_registry_icons() -> (Vec<RegistryIcon>, Vec<String>) {
    use windows::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_READ, RRF_RT_REG_BINARY, RegCloseKey, RegEnumKeyExW,
        RegOpenKeyExW,
    };
    let mut keys = Vec::new();
    unsafe {
        let mut hkey = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, &HSTRING::from(NOTIFY_ICON_SETTINGS), None, KEY_READ, &mut hkey).is_ok() {
            let mut i = 0;
            loop {
                let mut name = [0u16; 256];
                let mut len = name.len() as u32;
                let r = RegEnumKeyExW(hkey, i, Some(windows::core::PWSTR(name.as_mut_ptr())), &mut len, None, None, None, None);
                if r.is_err() {
                    break;
                }
                keys.push(String::from_utf16_lossy(&name[..len as usize]));
                i += 1;
            }
            let _ = RegCloseKey(hkey);
        }
    }
    let entries = keys
        .into_iter()
        .filter_map(|key| {
            let sub = format!(r"{NOTIFY_ICON_SETTINGS}\{key}");
            let exe = resolve_known_folder(&reg_string(&sub, "ExecutablePath")?).to_lowercase();
            Some(RegistryIcon {
                guid: reg_string(&sub, "IconGuid")
                    .and_then(|g| GUID::try_from(g.trim_matches(|c| c == '{' || c == '}')).ok()),
                uid: reg_dword(&sub, "UID"),
                promoted: reg_dword(&sub, "IsPromoted").is_some_and(|v| v != 0),
                exe,
                key,
            })
        })
        .collect();
    // display order: array of u64 registry key names
    let mut buf = vec![0u8; 8 * 512];
    let order = reg_value(NOTIFY_ICON_SETTINGS, "UIOrderList", RRF_RT_REG_BINARY, &mut buf)
        .map(|n| {
            buf[..n]
                .chunks_exact(8)
                .map(|c| u64::from_le_bytes(c.try_into().unwrap_or([0; 8])).to_string())
                .collect()
        })
        .unwrap_or_default();
    (entries, order)
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
                            promoted: false,
                            order: usize::MAX,
                            exe: None,
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

    /// Parses a forwarded `Shell_NotifyIconGetRect` request: (icon index, wants bottom-right).
    pub fn identify(&self, bytes: &[u8]) -> Option<(usize, bool)> {
        let size = std::mem::size_of::<IconIdentifierMessage>();
        if bytes.len() < size {
            return None;
        }
        let m: IconIdentifierMessage = unsafe { std::ptr::read_unaligned(bytes.as_ptr() as *const _) };
        let guid = (m.guid != GUID::zeroed()).then_some(m.guid);
        let idx = self.find(m.hwnd as i32 as isize, m.uid, guid)?;
        Some((idx, m.message == 2))
    }

    /// Owner process of an icon.
    pub fn owner_pid(&self, idx: usize) -> Option<u32> {
        let icon = self.icons.get(idx)?;
        let mut pid = 0;
        unsafe { GetWindowThreadProcessId(HWND(icon.hwnd as *mut _), Some(&mut pid)) };
        (pid != 0).then_some(pid)
    }

    /// Drops icons whose owner window is gone (the app died without removing them).
    pub fn prune(&mut self) -> bool {
        let before = self.icons.len();
        self.icons
            .retain(|i| unsafe { IsWindow(Some(HWND(i.hwnd as *mut _))).as_bool() });
        before != self.icons.len()
    }

    fn visible(&self) -> impl Iterator<Item = (usize, &TrayIcon)> {
        self.icons
            .iter()
            .enumerate()
            .filter(|(_, i)| !i.hidden && i.image.is_some())
    }

    /// Indices of icons shown on the bar itself.
    pub fn shown(&self, show_all: bool) -> Vec<usize> {
        self.visible()
            .filter(|(_, i)| show_all || i.promoted)
            .map(|(i, _)| i)
            .collect()
    }

    /// Indices of icons that live in the overflow flyout.
    pub fn overflow(&self, show_all: bool) -> Vec<usize> {
        if show_all {
            return Vec::new();
        }
        self.visible().filter(|(_, i)| !i.promoted).map(|(i, _)| i).collect()
    }

    /// Re-reads Windows' per-icon settings: which icons show on the taskbar, and
    /// their order. Matching follows Seelen UI: by icon GUID, else by exe + uID.
    /// Icons without a settings entry stay in the overflow (Windows' default).
    /// Returns true if anything changed.
    pub fn refresh_settings(&mut self) -> bool {
        let (entries, order) = read_registry_icons();
        let mut changed = false;
        for icon in &mut self.icons {
            if icon.exe.is_none() {
                let mut pid = 0;
                unsafe { GetWindowThreadProcessId(HWND(icon.hwnd as *mut _), Some(&mut pid)) };
                icon.exe = util::process_path(pid).map(|p| p.to_lowercase());
            }
            let entry = entries.iter().find(|e| match icon.guid {
                Some(g) => e.guid == Some(g),
                None => e.guid.is_none() && e.uid == Some(icon.uid) && icon.exe.as_deref() == Some(e.exe.as_str()),
            });
            let promoted = entry.is_some_and(|e| e.promoted);
            let pos = entry
                .and_then(|e| order.iter().position(|k| *k == e.key))
                .unwrap_or(usize::MAX);
            if promoted != icon.promoted || pos != icon.order {
                icon.promoted = promoted;
                icon.order = pos;
                changed = true;
            }
        }
        if changed {
            // stable: icons Windows doesn't know keep their arrival order at the end
            self.icons.sort_by_key(|i| i.order);
        }
        changed
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
    let src = exe.with_file_name("docksy_trayhook.dll");
    if !src.exists() {
        return Err(format!("{} not found", src.display()));
    }
    // Explorer keeps the hooked DLL loaded for a while, which would lock the build
    // output. Load a per-run copy instead and clean up copies that are free now.
    // The copy lives next to the exe: explorer silently refuses hook DLLs from the
    // user profile (%LOCALAPPDATA%) on Windows 11 build 28000.
    let dir = src.with_file_name("hook");
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
    let dll = dir.join(format!("docksy_trayhook_{stamp}.dll"));
    std::fs::copy(&src, &dll).map_err(|e| e.to_string())?;
    unsafe {
        let module = LoadLibraryW(&HSTRING::from(dll.as_os_str())).map_err(|e| e.to_string())?;
        let proc = GetProcAddress(module, s!("CallWndProc")).ok_or("CallWndProc export missing")?;
        let tray = crate::shell::native_taskbars()
            .into_iter()
            .find(|&h| util::window_class(h) == "Shell_TrayWnd")
            .ok_or("Shell_TrayWnd not found")?;
        let tid = GetWindowThreadProcessId(tray, None);
        crate::log!("hooking tray {:?} thread {tid} with {}", tray.0, dll.display());
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
