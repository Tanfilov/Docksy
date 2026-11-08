//! Seelena: a lightweight, native taskbar for Windows 11 (ARM64 and x64).
//! Window/tray/pin logic is ported from Seelen UI (AGPL-3.0); see README.md.

mod apps;
mod bar;
mod config;
mod icons;
mod pins;
mod shell;
mod tray;
mod util;

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    hash::{Hash, Hasher},
    sync::atomic::{AtomicBool, AtomicIsize, Ordering},
    time::{Duration, Instant},
};

use windows::Win32::{
    Foundation::{
        ERROR_ALREADY_EXISTS, GetLastError, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
    },
    Globalization::{
        DATE_LONGDATE, DATE_SHORTDATE, ENUM_DATE_FORMATS_FLAGS, GetDateFormatEx, GetTimeFormatEx,
        TIME_FORMAT_FLAGS, TIME_NOSECONDS,
    },
    Graphics::{
        Direct2D::{D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1CreateFactory, ID2D1Factory},
        DirectWrite::{DWRITE_FACTORY_TYPE_SHARED, DWriteCreateFactory, IDWriteFactory},
        Dwm::{
            DWM_SYSTEMBACKDROP_TYPE, DWMSBT_MAINWINDOW, DWMSBT_NONE, DWMSBT_TRANSIENTWINDOW,
            DWMWA_SYSTEMBACKDROP_TYPE, DWMWA_USE_IMMERSIVE_DARK_MODE,
            DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND, DwmExtendFrameIntoClientArea,
            DwmSetWindowAttribute,
        },
        Gdi::{
            BeginPaint, EndPaint, EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR,
            InvalidateRect, MONITORINFO, PAINTSTRUCT,
        },
    },
    System::{
        Com::{COINIT_APARTMENTTHREADED, CoInitializeEx},
        DataExchange::COPYDATASTRUCT,
        LibraryLoader::GetModuleHandleW,
        Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS},
        Registry::{
            HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW,
            RegSetKeyValueW,
        },
        Threading::CreateMutexW,
    },
    UI::{
        Accessibility::{HWINEVENTHOOK, SetWinEventHook},
        Controls::{ICC_BAR_CLASSES, ICC_WIN95_CLASSES, INITCOMMONCONTROLSEX, InitCommonControlsEx, MARGINS},
        HiDpi::{
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForMonitor, MDT_EFFECTIVE_DPI,
            SetProcessDpiAwarenessContext,
        },
        Input::KeyboardAndMouse::{
            TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent, VIRTUAL_KEY, VK_A, VK_D, VK_LWIN, VK_MENU,
            VK_N, VK_X,
        },
        Shell::{ABN_FULLSCREENAPP, ABN_POSCHANGED},
        WindowsAndMessaging::*,
    },
};
use windows::core::{BOOL, HSTRING, PCWSTR, w};

use apps::{Group, WindowTracker};
use bar::{Bar, Hit, PaintCtx};
use config::{Backdrop, Config, Position};
use icons::IconCache;
use tray::{Tray, TrayAction, TrayHook};

const WM_APPBAR: u32 = WM_APP + 1;
const WM_REPAINT: u32 = WM_APP + 2;
const WM_MOUSELEAVE_: u32 = 0x02A3;

const T_RESCAN: usize = 1;
const T_CLOCK: usize = 2;
const T_CONFIG: usize = 3;
const T_MAINT: usize = 4;
const T_REBUILD: usize = 5;
const T_EXPLORER: usize = 6;
const T_REPOSITION: usize = 7;

static RECEIVER: AtomicIsize = AtomicIsize::new(0);
static RESCAN_PENDING: AtomicBool = AtomicBool::new(false);
static FG_DIRTY: AtomicBool = AtomicBool::new(false);
static HIDE_NATIVE: AtomicBool = AtomicBool::new(false);

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

struct App {
    cfg: Config,
    cfg_mtime: Option<std::time::SystemTime>,
    bars: Vec<Bar>,
    tracker: WindowTracker,
    groups: Vec<Group>,
    groups_sig: u64,
    order: HashMap<String, u64>,
    next_order: u64,
    pins: Vec<pins::Pin>,
    flashing: HashSet<isize>,
    tray: Tray,
    tray_hook: Option<TrayHook>,
    icons: IconCache,
    d2d: ID2D1Factory,
    dw: IDWriteFactory,
    native_hidden: bool,
    last_broadcast: Instant,
    shell_hook_msg: u32,
    taskbar_created_msg: u32,
    time: String,
    date: String,
    long_date: String,
    quick: String,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

/// Runs `f` with the app state. Returns None when the state is already borrowed
/// (a nested message pump re-entered a window procedure).
fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|a| {
        let mut guard = a.try_borrow_mut().ok()?;
        guard.as_mut().map(f)
    })
}

fn receiver() -> HWND {
    HWND(RECEIVER.load(Ordering::Relaxed) as *mut _)
}

fn schedule(timer: usize, ms: u32) {
    unsafe { SetTimer(Some(receiver()), timer, ms, None) };
}

fn schedule_rescan() {
    if !RESCAN_PENDING.swap(true, Ordering::AcqRel) {
        schedule(T_RESCAN, 40);
    }
}

// ---------------------------------------------------------------------------
// monitors
// ---------------------------------------------------------------------------

struct Monitor {
    rect: RECT,
    primary: bool,
    scale: f32,
}

fn monitors() -> Vec<Monitor> {
    let mut v: Vec<Monitor> = Vec::new();
    unsafe extern "system" fn cb(h: HMONITOR, _: HDC, _: *mut RECT, lp: LPARAM) -> BOOL {
        let v = unsafe { &mut *(lp.0 as *mut Vec<Monitor>) };
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if unsafe { GetMonitorInfoW(h, &mut mi) }.as_bool() {
            let (mut dx, mut dy) = (96u32, 96u32);
            let _ = unsafe { GetDpiForMonitor(h, MDT_EFFECTIVE_DPI, &mut dx, &mut dy) };
            v.push(Monitor {
                rect: mi.rcMonitor,
                primary: mi.dwFlags & 1 != 0,
                scale: dx as f32 / 96.0,
            });
        }
        BOOL(1)
    }
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(cb), LPARAM(&mut v as *mut _ as isize));
    }
    v
}

// ---------------------------------------------------------------------------
// clock / quick settings text
// ---------------------------------------------------------------------------

fn format_time(fmt: &str) -> String {
    let mut buf = [0u16; 128];
    let f = HSTRING::from(fmt);
    let (flags, pf) = if fmt.is_empty() {
        (TIME_NOSECONDS, PCWSTR::null())
    } else {
        (TIME_FORMAT_FLAGS(0), PCWSTR(f.as_ptr()))
    };
    let n = unsafe { GetTimeFormatEx(PCWSTR::null(), flags, None, pf, Some(&mut buf)) };
    String::from_utf16_lossy(&buf[..(n.max(1) - 1) as usize])
}

fn format_date(fmt: &str, long: bool) -> String {
    let mut buf = [0u16; 128];
    let f = HSTRING::from(fmt);
    let (flags, pf) = if fmt.is_empty() {
        (if long { DATE_LONGDATE } else { DATE_SHORTDATE }, PCWSTR::null())
    } else {
        (ENUM_DATE_FORMATS_FLAGS(0), PCWSTR(f.as_ptr()))
    };
    let n = unsafe { GetDateFormatEx(PCWSTR::null(), flags, None, pf, Some(&mut buf), PCWSTR::null()) };
    String::from_utf16_lossy(&buf[..(n.max(1) - 1) as usize])
}

/// Segoe Fluent Icons glyphs: network, volume, and battery when present.
fn quick_glyphs() -> String {
    let mut s = String::from("\u{E701}  \u{E767}");
    let mut ps = SYSTEM_POWER_STATUS::default();
    if unsafe { GetSystemPowerStatus(&mut ps) }.is_ok() && ps.BatteryFlag != 128 && ps.BatteryLifePercent <= 100 {
        let pct = ps.BatteryLifePercent as u32;
        let glyph = if ps.ACLineStatus == 1 {
            0xE85A + (pct / 10).min(10) // BatteryCharging0..10
        } else if pct >= 100 {
            0xE83F
        } else {
            0xE850 + pct / 10 // Battery0..9
        };
        s.push_str("  ");
        s.push(char::from_u32(glyph).unwrap_or(' '));
    }
    s
}

// ---------------------------------------------------------------------------
// autostart
// ---------------------------------------------------------------------------

fn autostart_enabled() -> bool {
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            &HSTRING::from(RUN_KEY),
            w!("Seelena"),
            RRF_RT_REG_SZ,
            None,
            None,
            None,
        )
        .is_ok()
    }
}

fn set_autostart(on: bool) {
    unsafe {
        if on {
            let exe = std::env::current_exe().unwrap_or_default();
            let val = util::wide(&format!("\"{}\"", exe.display()));
            let _ = RegSetKeyValueW(
                HKEY_CURRENT_USER,
                &HSTRING::from(RUN_KEY),
                w!("Seelena"),
                REG_SZ.0,
                Some(val.as_ptr() as *const _),
                (val.len() * 2) as u32,
            );
        } else {
            let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, &HSTRING::from(RUN_KEY), w!("Seelena"));
        }
    }
}

// ---------------------------------------------------------------------------
// window actions
// ---------------------------------------------------------------------------

fn activate(hwnd: HWND) {
    unsafe {
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindowAsync(hwnd, SW_RESTORE);
        }
        if !SetForegroundWindow(hwnd).as_bool() {
            // the foreground lock is released by a key event from us
            util::send_keys(&[VK_MENU]);
            let _ = SetForegroundWindow(hwnd);
        }
    }
}

fn toggle(hwnd: HWND) {
    unsafe {
        if GetForegroundWindow() == hwnd && !IsIconic(hwnd).as_bool() {
            let _ = ShowWindowAsync(hwnd, SW_MINIMIZE);
        } else {
            activate(hwnd);
        }
    }
}

/// Several windows: focus the most recent one, or the next one if the app is already focused.
fn cycle(windows: &[(HWND, usize)]) {
    let fg = unsafe { GetForegroundWindow() };
    if let Some(i) = windows.iter().position(|(h, _)| *h == fg) {
        activate(windows[(i + 1) % windows.len()].0);
    } else if let Some((h, _)) = windows.iter().min_by_key(|(_, z)| *z) {
        activate(*h);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Btn {
    Left,
    Double,
    Right,
    Middle,
}

enum Act {
    None,
    Keys(Vec<VIRTUAL_KEY>),
    Toggle(HWND),
    Cycle(Vec<(HWND, usize)>),
    Launch(String),
    AppMenu {
        name: String,
        target: String,
        pinned: Option<String>,
        windows: Vec<(HWND, String)>,
    },
    BarMenu,
}

impl App {
    fn new() -> App {
        let d2d: ID2D1Factory =
            unsafe { D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None) }.expect("Direct2D");
        let dw: IDWriteFactory = unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED) }.expect("DirectWrite");
        let cfg = config::load();
        App {
            cfg_mtime: config::mtime(),
            cfg,

            bars: Vec::new(),
            tracker: WindowTracker::default(),
            groups: Vec::new(),
            groups_sig: 0,
            order: HashMap::new(),
            next_order: 0,
            pins: pins::load(),
            flashing: HashSet::new(),
            tray: Tray::default(),
            tray_hook: None,
            icons: IconCache::default(),
            d2d,
            dw,
            native_hidden: false,
            last_broadcast: Instant::now() - Duration::from_secs(60),
            shell_hook_msg: unsafe { RegisterWindowMessageW(w!("SHELLHOOK")) },
            taskbar_created_msg: tray::taskbar_created_msg(),
            time: String::new(),
            date: String::new(),
            long_date: String::new(),
            quick: String::new(),
        }
    }

    fn invalidate_all(&self) {
        for b in &self.bars {
            unsafe {
                let _ = InvalidateRect(Some(b.hwnd), None, false);
            }
        }
    }

    fn bar_index(&self, hwnd: HWND) -> Option<usize> {
        self.bars.iter().position(|b| b.hwnd == hwnd)
    }

    fn update_clock(&mut self) -> bool {
        let time = format_time(&self.cfg.time_format);
        let date = if self.cfg.show_date { format_date(&self.cfg.date_format, false) } else { String::new() };
        let quick = quick_glyphs();
        let changed = time != self.time || date != self.date || quick != self.quick;
        if changed {
            self.time = time;
            self.date = date;
            self.quick = quick;
            self.long_date = format_date("", true);
        }
        changed
    }

    fn set_native_hidden(&mut self, hide: bool) {
        HIDE_NATIVE.store(hide, Ordering::Relaxed);
        if hide {
            shell::hide_native();
        } else if self.native_hidden {
            shell::restore_native();
        }
        self.native_hidden = hide;
    }

    fn install_tray(&mut self) {
        self.tray_hook = None;
        match tray::install_hook() {
            Ok(h) => {
                self.tray_hook = Some(h);
                self.last_broadcast = Instant::now();
                tray::broadcast_taskbar_created();
                log!("tray hook installed");
            }
            Err(e) => log!("tray hook failed: {e}"),
        }
    }

    fn rescan(&mut self) {
        let windows = self.tracker.scan();
        self.groups = apps::build_groups(&self.pins, &windows, &mut self.order, &mut self.next_order);
        self.flashing
            .retain(|h| windows.iter().any(|w| w.hwnd.0 as isize == *h));
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for g in &self.groups {
            g.key.hash(&mut hasher);
            for w in &g.windows {
                (w.hwnd.0 as isize, &w.title, w.iconic).hash(&mut hasher);
            }
        }
        let sig = hasher.finish();
        if sig != self.groups_sig || FG_DIRTY.swap(false, Ordering::AcqRel) {
            self.groups_sig = sig;
            self.invalidate_all();
        }
    }

    fn destroy_bars(&mut self) {
        for b in self.bars.drain(..) {
            shell::appbar_remove(b.hwnd);
            unsafe {
                let _ = DestroyWindow(b.hwnd);
            }
        }
    }

    fn rebuild_bars(&mut self) {
        self.destroy_bars();
        let hinst = unsafe { GetModuleHandleW(None) }.unwrap_or_default();
        for m in monitors() {
            let thickness = (self.cfg.height * m.scale).round() as i32;
            let hwnd = unsafe {
                CreateWindowExW(
                    WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
                    w!("SeelenaBar"),
                    w!("Seelena"),
                    WS_POPUP,
                    m.rect.left,
                    m.rect.bottom - thickness,
                    m.rect.right - m.rect.left,
                    thickness,
                    None,
                    None,
                    Some(hinst.into()),
                    None,
                )
            };
            let Ok(hwnd) = hwnd else { continue };
            apply_dwm(hwnd, self.cfg.backdrop);
            shell::appbar_register(hwnd, WM_APPBAR);
            let rect = shell::appbar_set_pos(hwnd, self.cfg.position, m.rect, thickness, None);
            unsafe {
                let _ = SetWindowPos(
                    hwnd,
                    Some(HWND_TOPMOST),
                    rect.left,
                    rect.top,
                    rect.right - rect.left,
                    rect.bottom - rect.top,
                    SWP_NOACTIVATE | SWP_SHOWWINDOW,
                );
            }
            self.bars.push(Bar::new(hwnd, m.rect, m.primary, m.scale, rect));
        }
        self.invalidate_all();
    }

    fn reposition(&mut self) {
        for b in &mut self.bars {
            let thickness = (self.cfg.height * b.scale).round() as i32;
            let rect = shell::appbar_set_pos(b.hwnd, self.cfg.position, b.mon_rect, thickness, Some(b.rect));
            if rect != b.rect {
                b.rect = rect;
                unsafe {
                    let _ = SetWindowPos(
                        b.hwnd,
                        Some(HWND_TOPMOST),
                        rect.left,
                        rect.top,
                        rect.right - rect.left,
                        rect.bottom - rect.top,
                        SWP_NOACTIVATE,
                    );
                    let _ = InvalidateRect(Some(b.hwnd), None, false);
                }
            }
        }
    }

    fn reload_config(&mut self) {
        let new = config::load();
        let old = std::mem::replace(&mut self.cfg, new);
        if old.height != self.cfg.height || old.position != self.cfg.position || old.backdrop != self.cfg.backdrop {
            self.rebuild_bars();
        }
        if old.hide_windows_taskbar != self.cfg.hide_windows_taskbar {
            self.set_native_hidden(self.cfg.hide_windows_taskbar);
        }
        self.update_clock();
        self.time.clear(); // force re-render of formats
        self.update_clock();
        self.invalidate_all();
    }

    fn reload_pins(&mut self) {
        self.pins = pins::load();
        self.rescan();
        self.invalidate_all();
    }

    fn paint(&mut self, hwnd: HWND) {
        let Some(i) = self.bar_index(hwnd) else { return };
        let fg = unsafe { GetForegroundWindow() };
        let mut ctx = PaintCtx {
            cfg: &self.cfg,
            groups: &self.groups,
            tray: &self.tray,
            flashing: &self.flashing,
            icons: &mut self.icons,
            d2d: &self.d2d,
            dw: &self.dw,
            fg,
            time: &self.time,
            date: &self.date,
            long_date: &self.long_date,
            quick: &self.quick,
        };
        self.bars[i].paint(&mut ctx);
    }

    fn mouse_move(&mut self, hwnd: HWND, x: i32, y: i32) {
        let Some(i) = self.bar_index(hwnd) else { return };
        let bar = &mut self.bars[i];
        if !bar.tracking {
            let mut tme = TRACKMOUSEEVENT {
                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            unsafe {
                let _ = TrackMouseEvent(&mut tme);
            }
            bar.tracking = true;
        }
        let hit = bar.hit_test(x, y);
        if hit != bar.hover {
            if let Some(Hit::Tray(t)) = bar.hover {
                self.tray.send(t, TrayAction::HoverLeave);
            }
            if let Some(Hit::Tray(t)) = hit {
                self.tray.send(t, TrayAction::HoverEnter);
            }
            bar.hover = hit;
            unsafe {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
        }
        if let Some(Hit::Tray(t)) = hit {
            self.tray.send(t, TrayAction::Move);
        }
    }

    fn mouse_leave(&mut self, hwnd: HWND) {
        let Some(i) = self.bar_index(hwnd) else { return };
        let bar = &mut self.bars[i];
        if let Some(Hit::Tray(t)) = bar.hover {
            self.tray.send(t, TrayAction::HoverLeave);
        }
        bar.tracking = false;
        bar.hover = None;
        bar.pressed = None;
        unsafe {
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    }

    fn group_target(&self, g: &Group) -> String {
        if let Some(p) = g.pin.and_then(|i| self.pins.get(i)) {
            return p.path.clone();
        }
        match g.windows.first() {
            Some(w) if w.is_packaged() && w.aumid.is_some() => {
                format!("shell:AppsFolder\\{}", w.aumid.as_deref().unwrap_or_default())
            }
            Some(w) => w.exe.clone(),
            None => String::new(),
        }
    }

    fn action(&mut self, hit: Option<Hit>, btn: Btn) -> Act {
        use Btn::*;
        match (hit, btn) {
            (Some(Hit::Tray(i)), b) => {
                let a = match b {
                    Left => TrayAction::Left,
                    Double => TrayAction::Double,
                    Right => TrayAction::Right,
                    Middle => TrayAction::Middle,
                };
                self.tray.send(i, a);
                Act::None
            }
            (Some(Hit::Start), Left) => Act::Keys(vec![VK_LWIN]),
            (Some(Hit::Start), Right) => Act::Keys(vec![VK_LWIN, VK_X]),
            (Some(Hit::Quick), Left) => Act::Keys(vec![VK_LWIN, VK_A]),
            (Some(Hit::Clock), Left) => Act::Keys(vec![VK_LWIN, VK_N]),
            (Some(Hit::Desktop), Left) => Act::Keys(vec![VK_LWIN, VK_D]),
            (Some(Hit::App(i)), b) => {
                let Some(g) = self.groups.get(i) else { return Act::None };
                let target = self.group_target(g);
                match b {
                    Left | Double => match g.windows.len() {
                        0 => Act::Launch(target),
                        1 => Act::Toggle(g.windows[0].hwnd),
                        _ => Act::Cycle(g.windows.iter().map(|w| (w.hwnd, w.z)).collect()),
                    },
                    Middle => Act::Launch(target),
                    Right => Act::AppMenu {
                        name: g.name.clone(),
                        pinned: g.pin.and_then(|p| self.pins.get(p)).map(|p| p.path.clone()),
                        target,
                        windows: g.windows.iter().map(|w| (w.hwnd, w.title.clone())).collect(),
                    },
                }
            }
            (_, Right) => Act::BarMenu,
            _ => Act::None,
        }
    }

    fn pin(&mut self, path: &str) {
        let mut paths: Vec<String> = self.pins.iter().map(|p| p.path.clone()).collect();
        if !paths.iter().any(|p| p.eq_ignore_ascii_case(path)) {
            paths.push(path.to_string());
            pins::save(&paths);
        }
        self.reload_pins();
    }

    fn unpin(&mut self, path: &str) {
        let paths: Vec<String> = self
            .pins
            .iter()
            .map(|p| p.path.clone())
            .filter(|p| !p.eq_ignore_ascii_case(path))
            .collect();
        pins::save(&paths);
        self.reload_pins();
    }

    fn restart(&mut self) {
        self.cfg = config::load();
        self.icons.clear();
        self.tray.icons.clear();
        self.pins = pins::load();
        self.set_native_hidden(self.cfg.hide_windows_taskbar);
        self.rebuild_bars();
        self.install_tray();
        self.rescan();
    }

    fn shutdown(&mut self) {
        self.tray_hook = None;
        self.destroy_bars();
        if self.native_hidden {
            shell::restore_native();
            self.native_hidden = false;
        }
    }
}

fn apply_dwm(hwnd: HWND, backdrop: Backdrop) {
    unsafe {
        let corner = DWMWCP_DONOTROUND;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &corner as *const _ as *const _,
            std::mem::size_of_val(&corner) as u32,
        );
        let dark = BOOL(1);
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            &dark as *const _ as *const _,
            std::mem::size_of::<BOOL>() as u32,
        );
        let kind: DWM_SYSTEMBACKDROP_TYPE = match backdrop {
            Backdrop::Acrylic => DWMSBT_TRANSIENTWINDOW,
            Backdrop::Mica => DWMSBT_MAINWINDOW,
            Backdrop::None => DWMSBT_NONE,
        };
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_SYSTEMBACKDROP_TYPE,
            &kind as *const _ as *const _,
            std::mem::size_of_val(&kind) as u32,
        );
        let m = MARGINS {
            cxLeftWidth: -1,
            cxRightWidth: -1,
            cyTopHeight: -1,
            cyBottomHeight: -1,
        };
        let _ = DwmExtendFrameIntoClientArea(hwnd, &m);
    }
}

// ---------------------------------------------------------------------------
// menus (run outside of the app borrow: TrackPopupMenu pumps messages)
// ---------------------------------------------------------------------------

fn popup(owner: HWND, items: &[(usize, String, bool)]) -> usize {
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return 0 };
        for (id, text, checked) in items {
            if *id == 0 {
                let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            } else {
                let t = HSTRING::from(text.as_str());
                let flags = if *checked { MF_STRING | MF_CHECKED } else { MF_STRING };
                let _ = AppendMenuW(menu, flags, *id, &t);
            }
        }
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let _ = SetForegroundWindow(owner);
        let bottom = with_app(|a| a.cfg.position == Position::Bottom).unwrap_or(true);
        let align = if bottom { TPM_BOTTOMALIGN } else { TPM_TOPALIGN };
        let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | align, pt.x, pt.y, None, owner, None);
        let _ = PostMessageW(Some(owner), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        cmd.0 as usize
    }
}

fn run_action(owner: HWND, act: Act) {
    match act {
        Act::None => {}
        Act::Keys(k) => util::send_keys(&k),
        Act::Toggle(h) => toggle(h),
        Act::Cycle(list) => cycle(&list),
        Act::Launch(t) => {
            if !t.is_empty() {
                util::shell_open(&t, None);
            }
        }
        Act::AppMenu { name, target, pinned, windows } => {
            let mut items: Vec<(usize, String, bool)> = windows
                .iter()
                .enumerate()
                .take(20)
                .map(|(i, (_, t))| (10 + i, t.clone(), false))
                .collect();
            if !items.is_empty() {
                items.push((0, String::new(), false));
            }
            items.push((1, format!("Open {name}"), false));
            if pinned.is_some() {
                items.push((2, "Unpin from taskbar".into(), false));
            } else if !target.is_empty() {
                items.push((3, "Pin to taskbar".into(), false));
            }
            if !windows.is_empty() {
                let label = if windows.len() == 1 { "Close window" } else { "Close all windows" };
                items.push((4, label.into(), false));
            }
            match popup(owner, &items) {
                1 => util::shell_open(&target, None),
                2 => {
                    if let Some(p) = pinned {
                        with_app(|a| a.unpin(&p));
                    }
                }
                3 => {
                    with_app(|a| a.pin(&target));
                }
                4 => {
                    for (h, _) in &windows {
                        unsafe {
                            let _ = PostMessageW(Some(*h), WM_CLOSE, WPARAM(0), LPARAM(0));
                        }
                    }
                }
                n if n >= 10 => {
                    if let Some((h, _)) = windows.get(n - 10) {
                        activate(*h);
                    }
                }
                _ => {}
            }
        }
        Act::BarMenu => {
            let auto = autostart_enabled();
            let items = vec![
                (1, "Task Manager".to_string(), false),
                (0, String::new(), false),
                (2, "Taskbar settings (config.toml)".to_string(), false),
                (3, "Edit pinned apps (pins.txt)".to_string(), false),
                (4, "Start with Windows".to_string(), auto),
                (0, String::new(), false),
                (5, "Restart taskbar".to_string(), false),
                (6, "Exit and restore Windows taskbar".to_string(), false),
            ];
            match popup(owner, &items) {
                1 => util::shell_open("taskmgr.exe", None),
                2 => util::shell_open("notepad.exe", Some(&format!("\"{}\"", config::path().display()))),
                3 => util::shell_open(
                    "notepad.exe",
                    Some(&format!("\"{}\"", util::data_dir().join("pins.txt").display())),
                ),
                4 => set_autostart(!auto),
                5 => {
                    with_app(|a| a.restart());
                }
                6 => unsafe { PostQuitMessage(0) },
                _ => {}
            }
        }
    }
}

// ---------------------------------------------------------------------------
// window procedures
// ---------------------------------------------------------------------------

fn lparam_xy(lp: LPARAM) -> (i32, i32) {
    ((lp.0 & 0xFFFF) as i16 as i32, ((lp.0 >> 16) & 0xFFFF) as i16 as i32)
}

fn click(hwnd: HWND, lp: LPARAM, btn: Btn) {
    let (x, y) = lparam_xy(lp);
    let act = with_app(|a| {
        let i = a.bar_index(hwnd)?;
        let hit = a.bars[i].hit_test(x, y);
        let pressed = a.bars[i].pressed.take();
        if btn != Btn::Double && pressed != hit {
            return None; // released somewhere else
        }
        Some(a.action(hit, btn))
    })
    .flatten();
    if let Some(act) = act {
        run_action(hwnd, act);
    }
}

fn press(hwnd: HWND, lp: LPARAM) {
    let (x, y) = lparam_xy(lp);
    with_app(|a| {
        if let Some(i) = a.bar_index(hwnd) {
            a.bars[i].pressed = a.bars[i].hit_test(x, y);
        }
    });
}

unsafe extern "system" fn bar_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                BeginPaint(hwnd, &mut ps);
                let _ = EndPaint(hwnd, &ps);
                if with_app(|a| a.paint(hwnd)).is_none() {
                    let _ = PostMessageW(Some(hwnd), WM_REPAINT, WPARAM(0), LPARAM(0));
                }
                LRESULT(0)
            }
            WM_REPAINT => {
                let _ = InvalidateRect(Some(hwnd), None, false);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
            WM_MOUSEMOVE => {
                let (x, y) = lparam_xy(lp);
                with_app(|a| a.mouse_move(hwnd, x, y));
                LRESULT(0)
            }
            WM_MOUSELEAVE_ => {
                with_app(|a| a.mouse_leave(hwnd));
                LRESULT(0)
            }
            WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN => {
                press(hwnd, lp);
                LRESULT(0)
            }
            WM_LBUTTONDBLCLK => {
                // double clicks only matter for tray icons; elsewhere it is a second click
                let (x, y) = lparam_xy(lp);
                let is_tray = with_app(|a| {
                    let i = a.bar_index(hwnd)?;
                    let hit = a.bars[i].hit_test(x, y);
                    a.bars[i].swallow_up = matches!(hit, Some(Hit::Tray(_)));
                    Some(a.bars[i].swallow_up)
                })
                .flatten()
                .unwrap_or(false);
                if is_tray {
                    click(hwnd, lp, Btn::Double);
                } else {
                    press(hwnd, lp);
                }
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                let swallow = with_app(|a| {
                    a.bar_index(hwnd)
                        .map(|i| std::mem::take(&mut a.bars[i].swallow_up))
                        .unwrap_or(false)
                })
                .unwrap_or(false);
                if !swallow {
                    click(hwnd, lp, Btn::Left);
                }
                LRESULT(0)
            }
            WM_RBUTTONUP => {
                click(hwnd, lp, Btn::Right);
                LRESULT(0)
            }
            WM_MBUTTONUP => {
                click(hwnd, lp, Btn::Middle);
                LRESULT(0)
            }
            WM_APPBAR => {
                match wp.0 as u32 {
                    ABN_POSCHANGED => schedule(T_REPOSITION, 300),
                    ABN_FULLSCREENAPP => {
                        let after = if lp.0 != 0 { HWND_BOTTOM } else { HWND_TOPMOST };
                        let _ = SetWindowPos(hwnd, Some(after), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            WM_DISPLAYCHANGE | WM_DPICHANGED => {
                schedule(T_REBUILD, 800);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

unsafe extern "system" fn receiver_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_COPYDATA => {
                let Some(cds) = (lp.0 as *const COPYDATASTRUCT).as_ref() else {
                    return LRESULT(0);
                };
                if cds.dwData != tray::FORWARD_TAG || cds.lpData.is_null() {
                    return LRESULT(0);
                }
                let bytes = std::slice::from_raw_parts(cds.lpData as *const u8, cds.cbData as usize);
                with_app(|a| {
                    if a.tray.handle(bytes) {
                        a.invalidate_all();
                    }
                });
                LRESULT(1)
            }
            WM_TIMER => {
                let id = wp.0;
                match id {
                    T_RESCAN => {
                        let _ = KillTimer(Some(hwnd), T_RESCAN);
                        RESCAN_PENDING.store(false, Ordering::Release);
                        with_app(|a| a.rescan());
                    }
                    T_CLOCK => {
                        with_app(|a| {
                            if a.update_clock() {
                                a.invalidate_all();
                            }
                        });
                    }
                    T_CONFIG => {
                        with_app(|a| {
                            let m = config::mtime();
                            if m != a.cfg_mtime {
                                a.cfg_mtime = m;
                                a.reload_config();
                                log!("config reloaded");
                            }
                        });
                    }
                    T_MAINT => {
                        with_app(|a| {
                            if a.tray.prune() {
                                a.invalidate_all();
                            }
                            if a.native_hidden {
                                for h in shell::native_taskbars() {
                                    if IsWindowVisible(h).as_bool() {
                                        shell::rehide(h);
                                    }
                                }
                            }
                        });
                        schedule_rescan();
                    }
                    T_REBUILD => {
                        let _ = KillTimer(Some(hwnd), T_REBUILD);
                        with_app(|a| {
                            a.rebuild_bars();
                            a.rescan();
                        });
                    }
                    T_REPOSITION => {
                        let _ = KillTimer(Some(hwnd), T_REPOSITION);
                        with_app(|a| a.reposition());
                    }
                    T_EXPLORER => {
                        let _ = KillTimer(Some(hwnd), T_EXPLORER);
                        log!("explorer restarted");
                        with_app(|a| a.restart());
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            WM_DISPLAYCHANGE => {
                schedule(T_REBUILD, 800);
                LRESULT(0)
            }
            WM_ENDSESSION => {
                if wp.0 != 0 {
                    with_app(|a| a.shutdown());
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => {
                let (shell_msg, created_msg) =
                    with_app(|a| (a.shell_hook_msg, a.taskbar_created_msg)).unwrap_or((0, 0));
                if msg != 0 && msg == shell_msg {
                    let code = wp.0 as u32;
                    let target = lp.0;
                    with_app(|a| match code {
                        0x8006 => {
                            // HSHELL_FLASH
                            a.flashing.insert(target);
                            a.invalidate_all();
                        }
                        4 | 0x8004 => {
                            // HSHELL_WINDOWACTIVATED / HSHELL_RUDEAPPACTIVATED
                            if a.flashing.remove(&target) {
                                a.invalidate_all();
                            }
                        }
                        _ => {}
                    });
                    schedule_rescan();
                    return LRESULT(0);
                }
                if msg != 0 && msg == created_msg {
                    // explorer restarted (ignore the echo of our own broadcast)
                    let ours = with_app(|a| a.last_broadcast.elapsed() < Duration::from_secs(3)).unwrap_or(true);
                    if !ours {
                        schedule(T_EXPLORER, 1500);
                    }
                    return LRESULT(0);
                }
                DefWindowProcW(hwnd, msg, wp, lp)
            }
        }
    }
}

unsafe extern "system" fn win_event(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    id_child: i32,
    _thread: u32,
    _time: u32,
) {
    if id_object != 0 || id_child != 0 || hwnd.is_invalid() {
        return;
    }
    if event == EVENT_OBJECT_SHOW
        && HIDE_NATIVE.load(Ordering::Relaxed)
        && shell::TASKBAR_CLASSES.contains(&util::window_class(hwnd).as_str())
    {
        shell::rehide(hwnd);
        return;
    }
    if event == EVENT_SYSTEM_FOREGROUND {
        FG_DIRTY.store(true, Ordering::Release);
    }
    schedule_rescan();
}

fn register_class(name: PCWSTR, proc: WNDPROC, dblclks: bool) {
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap_or_default();
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: if dblclks { CS_DBLCLKS } else { WNDCLASS_STYLES(0) },
            lpfnWndProc: proc,
            hInstance: hinst.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: name,
            ..Default::default()
        };
        RegisterClassExW(&wc);
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--restore") {
        shell::restore_native();
        return;
    }
    if args.iter().any(|a| a == "--dump") {
        // diagnostics: how pins resolve and what windows we see, written to seelena.log
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        }
        for p in pins::load() {
            log!("pin {:?}", p);
        }
        for w in WindowTracker::default().scan() {
            log!("window {:?} exe={} aumid={:?} title={}", w.hwnd.0, w.exe, w.aumid, w.title);
        }
        return;
    }

    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _mutex = CreateMutexW(None, true, w!("Local\\SeelenaTaskbar"));
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return;
        }
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let icc = INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_BAR_CLASSES | ICC_WIN95_CLASSES,
        };
        let _ = InitCommonControlsEx(&icc);
    }

    std::panic::set_hook(Box::new(|info| {
        log!("panic: {info}");
        shell::restore_native();
    }));
    log!("starting");

    register_class(w!("SeelenaTrayReceiver"), Some(receiver_proc), false);
    register_class(w!("SeelenaBar"), Some(bar_proc), true);

    let recv = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
            w!("SeelenaTrayReceiver"),
            w!("Seelena"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(GetModuleHandleW(None).unwrap_or_default().into()),
            None,
        )
    }
    .expect("receiver window");
    RECEIVER.store(recv.0 as isize, Ordering::Relaxed);
    unsafe {
        // the hook DLL runs in explorer and sends WM_COPYDATA to us
        let _ = ChangeWindowMessageFilterEx(recv, WM_COPYDATA, MSGFLT_ALLOW, None);
        let _ = RegisterShellHookWindow(recv);
    }

    APP.with(|a| *a.borrow_mut() = Some(App::new()));
    with_app(|a| {
        a.update_clock();
        let hide = a.cfg.hide_windows_taskbar;
        a.set_native_hidden(hide);
        a.rebuild_bars();
        a.rescan();
        a.install_tray();
    });

    let flags = WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS;
    let ranges = [
        (EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND),
        (EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MINIMIZEEND),
        (EVENT_OBJECT_CREATE, EVENT_OBJECT_HIDE),
        (EVENT_OBJECT_NAMECHANGE, EVENT_OBJECT_NAMECHANGE),
        (EVENT_OBJECT_CLOAKED, EVENT_OBJECT_UNCLOAKED),
    ];
    let _hooks: Vec<HWINEVENTHOOK> = ranges
        .iter()
        .map(|&(lo, hi)| unsafe { SetWinEventHook(lo, hi, None, Some(win_event), 0, 0, flags) })
        .collect();

    schedule(T_CLOCK, 1000);
    schedule(T_CONFIG, 1500);
    schedule(T_MAINT, 5000);

    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    with_app(|a| a.shutdown());
    log!("exited");
}
