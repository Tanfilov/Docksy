//! Which windows belong on the taskbar, and how they group into buttons.
//! Filtering rules follow Seelen UI's `platform/window/windows/interactable.rs`.

use std::collections::HashMap;

use windows::Win32::{
    Foundation::{HWND, LPARAM},
    Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute},
    Storage::EnhancedStorage::PKEY_AppUserModel_ID,
    UI::{
        Shell::PropertiesSystem::{IPropertyStore, SHGetPropertyStoreForWindow},
        WindowsAndMessaging::{
            EnumWindows, GW_OWNER, GWL_EXSTYLE, GWL_STYLE, GetWindow, GetWindowLongPtrW,
            GetWindowThreadProcessId, IsIconic, IsWindowVisible, WS_CHILD, WS_EX_APPWINDOW,
            WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
        },
    },
};
use windows::core::BOOL;

use crate::{pins::Pin, util};

#[derive(Clone)]
pub struct AppWindow {
    pub hwnd: HWND,
    pub title: String,
    pub exe: String,
    pub aumid: Option<String>,
    pub iconic: bool,
    /// z-order rank at scan time (0 = topmost, i.e. most recently used)
    pub z: usize,
}

impl AppWindow {
    pub fn group_key(&self) -> String {
        match &self.aumid {
            Some(a) => a.to_lowercase(),
            None => self.exe.to_lowercase(),
        }
    }

    pub fn is_packaged(&self) -> bool {
        let e = self.exe.to_lowercase();
        e.contains("\\windowsapps\\") || e.ends_with("\\applicationframehost.exe")
    }
}

/// Per-window data that never changes during the window's life.
#[derive(Clone)]
struct Static {
    pid: u32,
    exe: String,
    aumid: Option<String>,
}

#[derive(Default)]
pub struct WindowTracker {
    cache: HashMap<isize, Static>,
}

const IGNORED_CLASSES: &[&str] = &[
    "Progman",
    "WorkerW",
    "Shell_TrayWnd",
    "Shell_SecondaryTrayWnd",
    "Windows.Internal.Shell.TabProxyWindow",
    "DocksyBar",
];

fn is_cloaked(hwnd: HWND) -> bool {
    let mut cloaked = 0u32;
    unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut _ as *mut _,
            std::mem::size_of::<u32>() as u32,
        )
        .is_ok()
            && cloaked != 0
    }
}

fn window_aumid(hwnd: HWND) -> Option<String> {
    unsafe {
        let store: IPropertyStore = SHGetPropertyStoreForWindow(hwnd).ok()?;
        let v = store.GetValue(&PKEY_AppUserModel_ID).ok()?;
        let s = v.to_string();
        (!s.is_empty()).then_some(s)
    }
}

pub(crate) fn is_taskbar_window(hwnd: HWND) -> bool {
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() || is_cloaked(hwnd) {
            return false;
        }
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        if ex & WS_EX_APPWINDOW.0 == 0 {
            if style & WS_CHILD.0 != 0 {
                return false;
            }
            if GetWindow(hwnd, GW_OWNER).is_ok_and(|o| !o.is_invalid()) {
                return false;
            }
            if ex & (WS_EX_TOOLWINDOW.0 | WS_EX_NOACTIVATE.0) != 0 {
                return false;
            }
        }
    }
    let class = util::window_class(hwnd);
    !IGNORED_CLASSES.contains(&class.as_str())
}

impl WindowTracker {
    pub fn scan(&mut self) -> Vec<AppWindow> {
        let mut hwnds: Vec<HWND> = Vec::new();
        unsafe extern "system" fn cb(hwnd: HWND, lp: LPARAM) -> BOOL {
            let v = unsafe { &mut *(lp.0 as *mut Vec<HWND>) };
            v.push(hwnd);
            BOOL(1)
        }
        unsafe {
            let _ = EnumWindows(Some(cb), LPARAM(&mut hwnds as *mut _ as isize));
        }

        let own_pid = std::process::id();
        let mut out = Vec::new();
        let mut alive = std::collections::HashSet::new();
        for hwnd in hwnds {
            if !is_taskbar_window(hwnd) {
                continue;
            }
            let key = hwnd.0 as isize;
            alive.insert(key);
            let info = match self.cache.get(&key) {
                Some(i) => i.clone(),
                None => {
                    let mut pid = 0;
                    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
                    let Some(exe) = util::process_path(pid) else {
                        continue; // system process we can't manage
                    };
                    let info = Static {
                        pid,
                        exe,
                        aumid: window_aumid(hwnd).or_else(|| util::process_aumid(pid)),
                    };
                    self.cache.insert(key, info.clone());
                    info
                }
            };
            if info.pid == own_pid {
                continue;
            }
            let title = util::window_title(hwnd);
            if title.is_empty() {
                continue;
            }
            out.push(AppWindow {
                hwnd,
                title,
                exe: info.exe,
                aumid: info.aumid,
                iconic: unsafe { IsIconic(hwnd).as_bool() },
                z: out.len(),
            });
        }
        self.cache.retain(|k, _| alive.contains(k));
        out
    }
}

/// One taskbar button.
pub struct Group {
    pub key: String,
    pub name: String,
    pub pin: Option<usize>,
    pub windows: Vec<AppWindow>,
    /// icon sources, tried in order (shell parsing names / paths)
    pub icon_sources: Vec<String>,
}

fn pin_matches(pin: &Pin, w: &AppWindow) -> bool {
    let exe_eq = pin.exe.as_deref().is_some_and(|e| e.eq_ignore_ascii_case(&w.exe));
    match (&w.aumid, &pin.aumid) {
        (Some(wa), Some(pa)) => wa.eq_ignore_ascii_case(pa),
        (Some(_), None) => exe_eq,
        (None, _) => exe_eq,
    }
}

/// Builds buttons: pinned apps first (in pin order), then other running apps in
/// first-seen order (`order` keeps that order stable between scans).
pub fn build_groups(
    pins: &[Pin],
    windows: &[AppWindow],
    order: &mut HashMap<String, u64>,
    next_order: &mut u64,
) -> Vec<Group> {
    let mut groups: Vec<Group> = pins
        .iter()
        .enumerate()
        .map(|(i, p)| Group {
            key: format!("pin:{}", p.path.to_lowercase()),
            name: p.name.clone(),
            pin: Some(i),
            windows: Vec::new(),
            icon_sources: p.icon_sources(),
        })
        .collect();

    let mut running: Vec<Group> = Vec::new();
    for w in windows {
        if let Some(i) = pins.iter().position(|p| pin_matches(p, w)) {
            groups[i].windows.push(w.clone());
            continue;
        }
        let key = w.group_key();
        if let Some(g) = running.iter_mut().find(|g| g.key == key) {
            g.windows.push(w.clone());
            continue;
        }
        let mut icon_sources = Vec::new();
        if w.is_packaged() {
            if let Some(a) = &w.aumid {
                icon_sources.push(format!("shell:AppsFolder\\{a}"));
            }
        }
        icon_sources.push(w.exe.clone());
        running.push(Group {
            key: key.clone(),
            name: util::file_stem(&w.exe),
            pin: None,
            windows: vec![w.clone()],
            icon_sources,
        });
        order.entry(key).or_insert_with(|| {
            *next_order += 1;
            *next_order
        });
    }
    running.sort_by_key(|g| order.get(&g.key).copied().unwrap_or(u64::MAX));
    // forget the order of apps that closed, so they reappear at the end
    order.retain(|k, _| running.iter().any(|g| &g.key == k));

    // windows keep a stable order inside a group (z-order changes on every focus)
    for g in groups.iter_mut().chain(running.iter_mut()) {
        g.windows.sort_by_key(|w| w.hwnd.0 as isize);
    }
    groups.extend(running);
    groups
}
