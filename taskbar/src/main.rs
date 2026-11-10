//! Docksy: a lightweight, native taskbar for Windows 11 (ARM64 and x64).
//! Window/tray/pin logic is ported from Seelen UI (AGPL-3.0); see README.md.

mod apps;
mod backdrop;
mod bar;
mod flyout;
mod settings;
mod startmenu;
mod config;
mod icons;
mod lang;
mod monitor;
mod pins;
mod power;
mod screenoff;
mod powercfg;
mod powerpanel;
mod shell;
mod tray;
mod util;
mod winkey;

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
        Controls::{ICC_BAR_CLASSES, ICC_WIN95_CLASSES, INITCOMMONCONTROLSEX, InitCommonControlsEx},
        HiDpi::{
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForMonitor, MDT_EFFECTIVE_DPI,
            SetProcessDpiAwarenessContext,
        },
        Input::KeyboardAndMouse::{
            TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent, VIRTUAL_KEY, VK_A, VK_LWIN, VK_MENU,
            VK_N, VK_R, VK_S,
        },
        Shell::{ABN_FULLSCREENAPP, ABN_POSCHANGED},
        WindowsAndMessaging::*,
    },
};
use windows::core::{BOOL, HSTRING, PCWSTR, w};

use apps::{Group, WindowTracker};
use bar::{Bar, Hit, PaintCtx};
use config::{Config, Position, Side};
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
const T_TRAY_SETTINGS: usize = 8;
const WM_FLYOUT_CLOSE: u32 = WM_APP + 4;

/// Quits Docksy; shutdown() gives the screen edge back to the Windows taskbar.
const EXIT_LABEL: &str = "Close Docksy (back to Windows taskbar)";
const WM_APPS_READY: u32 = WM_APP + 5;
const WM_START_HIDE: u32 = WM_APP + 6;
const WM_SETTINGS_HIDE: u32 = WM_APP + 7;
const WM_WIN_START: u32 = WM_APP + 8;
/// background-loaded icons are ready (Start panel)
const WM_ICONS_READY: u32 = WM_APP + 9;
/// the battery panel lost focus
const WM_POWER_HIDE: u32 = WM_APP + 10;
const T_LIVE_RESIZE: usize = 10;
const T_MONITOR: usize = 11;
const T_LANG: usize = 12;
static LIVE_RESIZE_PENDING: AtomicBool = AtomicBool::new(false);
const WM_FLYOUT_HIDE: u32 = WM_APP + 3;

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
    /// windows the corner button minimized (top-most first), to restore on the next click
    minimized_by_us: Vec<HWND>,
    groups: Vec<Group>,
    groups_sig: u64,
    order: HashMap<String, u64>,
    next_order: u64,
    pins: Vec<pins::Pin>,
    flashing: HashSet<isize>,
    tray: Tray,
    tray_hook: Option<TrayHook>,
    flyout: Option<flyout::Flyout>,
    start: Option<startmenu::StartMenu>,
    settings: Option<settings::SettingsPanel>,
    /// battery panel: power plan, power mode, screen/sleep timers
    power: Option<powerpanel::PowerPanel>,
    /// a settings slider is being dragged
    live_resize: bool,
    icons: IconCache,
    d2d: ID2D1Factory,
    dw: IDWriteFactory,
    native_hidden: bool,
    last_broadcast: Instant,
    /// `Shell_TrayWnd` when the tray hook was installed; a new one means explorer restarted
    tray_wnd: isize,
    shell_hook_msg: u32,
    taskbar_created_msg: u32,
    time: String,
    date: String,
    long_date: String,
    quick: String,
    battery: Option<bar::Battery>,
    monitor: monitor::Monitor,
    stats: monitor::Stats,
    /// input language of the window that gets typing
    lang: Option<lang::InputLang>,
    /// last focused window that isn't ours: the one a language switch applies to
    lang_target: HWND,
    icon_loader: Option<icons::IconLoader>,
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

/// Segoe Fluent Icons glyphs: network, volume.
fn quick_glyphs() -> String {
    String::from("\u{E701}  \u{E767}")
}

/// Battery charge, or None on a PC without a battery.
fn battery_status() -> Option<bar::Battery> {
    let mut ps = SYSTEM_POWER_STATUS::default();
    unsafe { GetSystemPowerStatus(&mut ps) }.ok()?;
    if ps.BatteryFlag == 128 || ps.BatteryFlag == 255 || ps.BatteryLifePercent > 100 {
        return None;
    }
    Some(bar::Battery {
        percent: ps.BatteryLifePercent,
        plugged_in: ps.ACLineStatus == 1,
        charging: ps.BatteryFlag & 8 != 0,
    })
}

// ---------------------------------------------------------------------------
// autostart
// ---------------------------------------------------------------------------

/// `--quit`: asks the running Docksy to close (it gives the screen edge back to
/// the Windows taskbar) and waits up to 5 s for it. Used by the installer.
fn quit_running_instance() {
    unsafe {
        let Ok(receiver) = FindWindowW(w!("DocksyTrayReceiver"), PCWSTR::null()) else { return };
        let mut pid = 0u32;
        GetWindowThreadProcessId(receiver, Some(&mut pid));
        let _ = PostMessageW(Some(receiver), WM_CLOSE, WPARAM(0), LPARAM(0));
        if let Ok(process) = windows::Win32::System::Threading::OpenProcess(
            windows::Win32::System::Threading::PROCESS_SYNCHRONIZE,
            false,
            pid,
        ) {
            windows::Win32::System::Threading::WaitForSingleObject(process, 5000);
            let _ = windows::Win32::Foundation::CloseHandle(process);
        }
    }
}

fn autostart_enabled() -> bool {
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            &HSTRING::from(RUN_KEY),
            w!("Docksy"),
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
                w!("Docksy"),
                REG_SZ.0,
                Some(val.as_ptr() as *const _),
                (val.len() * 2) as u32,
            );
        } else {
            let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, &HSTRING::from(RUN_KEY), w!("Docksy"));
        }
    }
}

// ---------------------------------------------------------------------------
// window actions
// ---------------------------------------------------------------------------

// Windows of elevated apps (Task Manager, admin terminals, ...) reject ShowWindowAsync
// and WM_CLOSE from a normal-privilege process (UIPI), but accept WM_SYSCOMMAND.
// So minimize/restore/close always go through WM_SYSCOMMAND, like the title bar buttons.
fn syscommand(hwnd: HWND, cmd: u32) {
    unsafe {
        let _ = PostMessageW(Some(hwnd), WM_SYSCOMMAND, WPARAM(cmd as usize), LPARAM(0));
    }
}

fn activate(hwnd: HWND) {
    unsafe {
        if IsIconic(hwnd).as_bool() {
            syscommand(hwnd, SC_RESTORE);
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
            syscommand(hwnd, SC_MINIMIZE);
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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Btn {
    Left,
    Double,
    Right,
    Middle,
}

enum Act {
    None,
    /// keys, or open the URI when keys would be dropped (elevated app focused)
    KeysOr(Vec<VIRTUAL_KEY>, &'static str),
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
    /// right-click on Start: Docksy's own version of Windows' Win+X menu
    StartButtonMenu,
    /// input languages to switch to
    LangMenu,
    /// right-click on the CPU / GPU / RAM metrics: which end of the bar they sit at
    MonitorMenu,
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
            minimized_by_us: Vec::new(),
            groups: Vec::new(),
            groups_sig: 0,
            order: HashMap::new(),
            next_order: 0,
            pins: pins::load(),
            flashing: HashSet::new(),
            tray: Tray::default(),
            tray_hook: None,
            flyout: None,
            start: None,
            settings: None,
            power: None,
            live_resize: false,
            icons: IconCache::default(),
            d2d,
            dw,
            native_hidden: false,
            last_broadcast: Instant::now() - Duration::from_secs(60),
            tray_wnd: 0,
            shell_hook_msg: unsafe { RegisterWindowMessageW(w!("SHELLHOOK")) },
            taskbar_created_msg: tray::taskbar_created_msg(),
            time: String::new(),
            date: String::new(),
            long_date: String::new(),
            quick: String::new(),
            battery: None,
            monitor: monitor::Monitor::new(),
            stats: monitor::Stats::default(),
            lang: None,
            lang_target: HWND::default(),
            icon_loader: None,
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

    /// Follows the input language of the focused window (ours don't count: clicking
    /// the bar must not change what it shows).
    fn update_lang(&mut self) {
        let fg = unsafe { GetForegroundWindow() };
        if fg.is_invalid() {
            return;
        }
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(fg, Some(&mut pid)) };
        if pid == std::process::id() {
            return;
        }
        self.lang_target = fg;
        let hkl = lang::of_window(fg);
        if hkl == 0 || self.lang.as_ref().is_some_and(|l| l.hkl == hkl) {
            return;
        }
        self.lang = Some(lang::describe(hkl));
        if self.cfg.show_language {
            self.invalidate_all();
        }
    }

    fn update_clock(&mut self) -> bool {
        let time = format_time(&self.cfg.time_format);
        let date = if self.cfg.show_date { format_date(&self.cfg.date_format, false) } else { String::new() };
        let quick = quick_glyphs();
        let battery = battery_status();
        let changed = time != self.time || date != self.date || quick != self.quick || battery != self.battery;
        if changed {
            self.battery = battery;
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
        self.tray_wnd = primary_tray();
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

    /// Corner button. First click minimizes every open app window (not Windows'
    /// "show desktop" mode, which would also cover our bar); the next click restores
    /// exactly those windows, as long as none of them was restored in between.
    fn toggle_minimize_all(&mut self) {
        let restore = !self.minimized_by_us.is_empty()
            && self
                .minimized_by_us
                .iter()
                .all(|&h| unsafe { IsWindow(Some(h)).as_bool() && IsIconic(h).as_bool() });
        if restore {
            // restore bottom-most first, so the previous top window ends up on top
            let list = std::mem::take(&mut self.minimized_by_us);
            for &h in list.iter().rev() {
                syscommand(h, SC_RESTORE);
            }
            if let Some(&top) = list.first() {
                unsafe {
                    let _ = SetForegroundWindow(top);
                }
            }
            return;
        }
        // scan() returns windows in z-order: top-most first
        let windows: Vec<HWND> = self
            .tracker
            .scan()
            .into_iter()
            .filter(|w| !w.iconic)
            .map(|w| w.hwnd)
            .collect();
        for &h in &windows {
            syscommand(h, SC_MINIMIZE);
        }
        self.minimized_by_us = windows;
    }

    fn rescan(&mut self) {
        // runs after every window event: catch the native taskbar popping back up
        // (it does on app launch / attention requests) before it covers our bar
        if self.native_hidden {
            shell::enforce_hidden();
        }
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
        self.hide_start();
        self.hide_overflow();
        self.destroy_bars();
        let hinst = unsafe { GetModuleHandleW(None) }.unwrap_or_default();
        for m in monitors() {
            let thickness = (self.cfg.height * m.scale).round() as i32;
            let hwnd = unsafe {
                CreateWindowExW(
                    WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
                    w!("DocksyBar"),
                    w!("Docksy"),
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
            backdrop::apply(hwnd, &self.cfg, false);
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
        winkey::set_enabled(new.win_key_opens_start);
        let old = std::mem::replace(&mut self.cfg, new);
        if old.height != self.cfg.height
            || old.position != self.cfg.position
            || old.backdrop != self.cfg.backdrop
            || old.background != self.cfg.background
        {
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
            overflow_open: self.flyout.as_ref().is_some_and(|f| f.open),
            battery: self.battery,
            fast_icons: self.live_resize,
            stats: self.cfg.show_monitor.then_some(self.stats),
            lang: if self.cfg.show_language { self.lang.as_ref() } else { None },
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

    /// Opens the tray overflow flyout next to bar `bar`'s ^ button, or closes it.
    fn toggle_overflow(&mut self, bar: usize) {
        let Some(f) = self.flyout.as_mut() else { return };
        if f.open {
            f.hide();
        } else if f.closed_at.elapsed() > Duration::from_millis(250) {
            // (a click that just closed it by taking focus away must not reopen it)
            let b = &self.bars[bar];
            let Some(r) = b.items.iter().find(|(h, _)| *h == Hit::Overflow).map(|(_, r)| *r) else {
                return;
            };
            let button = RECT {
                left: b.rect.left + r.left as i32,
                top: b.rect.top + r.top as i32,
                right: b.rect.left + r.right as i32,
                bottom: b.rect.top + r.bottom as i32,
            };
            f.show(&self.cfg, &self.tray, button, b.rect, b.mon_rect, b.scale);
        }
        self.invalidate_all();
    }

    /// Opens the battery panel next to bar `bar`'s battery icon, or closes it.
    fn toggle_power(&mut self, bar: usize) {
        self.hide_start();
        self.hide_overflow();
        let Some(p) = self.power.as_mut() else { return };
        if p.open {
            p.hide();
            return;
        }
        // (a click that just closed it by taking focus away must not reopen it)
        if p.closed_at.elapsed() < Duration::from_millis(250) {
            return;
        }
        let b = &self.bars[bar];
        let Some(r) = b.items.iter().find(|(h, _)| *h == Hit::Battery).map(|(_, r)| *r) else {
            return;
        };
        let button = RECT {
            left: b.rect.left + r.left as i32,
            top: b.rect.top + r.top as i32,
            right: b.rect.left + r.right as i32,
            bottom: b.rect.top + r.bottom as i32,
        };
        p.show(&self.cfg, button, b.rect, b.mon_rect, b.scale);
    }

    /// Opens Docksy's Start panel next to bar `bar`'s Start button, or closes it.
    /// Win key pressed alone: toggle the Start panel on the bar of the monitor
    /// under the mouse (the primary bar otherwise).
    fn open_start_from_key(&mut self) {
        let mut pt = POINT::default();
        unsafe {
            let _ = GetCursorPos(&mut pt);
        }
        let on_monitor = |b: &Bar| {
            pt.x >= b.mon_rect.left && pt.x < b.mon_rect.right && pt.y >= b.mon_rect.top && pt.y < b.mon_rect.bottom
        };
        let Some(i) = self
            .bars
            .iter()
            .position(on_monitor)
            .or_else(|| self.bars.iter().position(|b| b.primary))
        else {
            return;
        };
        // a keyboard toggle must not be blocked by the "just closed" click guard
        if let Some(m) = self.start.as_mut() {
            if !m.open {
                m.closed_at = Instant::now() - Duration::from_secs(1);
            }
        }
        self.toggle_start(i);
    }

    fn toggle_start(&mut self, bar: usize) {
        self.hide_overflow();
        let Some(m) = self.start.as_mut() else { return };
        if m.open {
            m.hide();
            return;
        }
        // (a click that just closed it by taking focus away must not reopen it)
        if m.closed_at.elapsed() < Duration::from_millis(250) {
            return;
        }
        let b = &self.bars[bar];
        let Some(r) = b.items.iter().find(|(h, _)| *h == Hit::Start).map(|(_, r)| *r) else {
            return;
        };
        let button = RECT {
            left: b.rect.left + r.left as i32,
            top: b.rect.top + r.top as i32,
            right: b.rect.left + r.right as i32,
            bottom: b.rect.top + r.bottom as i32,
        };
        m.show(&self.cfg, button, b.rect, b.mon_rect, b.scale);
        if m.wants_refresh() {
            startmenu::refresh_apps_async(receiver(), WM_APPS_READY);
        }
    }

    /// Opens the taskbar settings panel on the monitor of bar `bar`.
    fn open_settings(&mut self, owner: HWND) {
        self.hide_start();
        self.hide_overflow();
        let (monitor, scale) = self
            .bars
            .iter()
            .find(|b| b.hwnd == owner)
            .or_else(|| self.bars.iter().find(|b| b.primary))
            .map(|b| (b.mon_rect, b.scale))
            .unwrap_or((RECT { left: 0, top: 0, right: 1280, bottom: 800 }, 1.0));
        if let Some(p) = self.settings.as_mut() {
            p.show(&self.cfg, monitor, scale);
        }
    }

    /// Applies a slider value live (not saved yet). Icons always keep a padding to the
    /// bar's edges: a shorter bar shrinks them, and they can't grow past it.
    fn set_slider(&mut self, slider: settings::Slider, value: f32) {
        use settings::Slider;
        let (min, max) = slider.range();
        let v = value.clamp(min, max);
        match slider {
            Slider::Height => {
                if (self.cfg.height - v).abs() < 0.5 {
                    return;
                }
                self.cfg.height = v;
                let max_icon = config::max_icon_size(v);
                self.cfg.icon_size = self.cfg.icon_size.min(max_icon);
                self.cfg.tray_icon_size = self.cfg.tray_icon_size.min(max_icon);
                // resizing the reserved screen edge makes windows reflow: throttle it
                if !LIVE_RESIZE_PENDING.swap(true, Ordering::AcqRel) {
                    schedule(T_LIVE_RESIZE, 60);
                }
            }
            Slider::Icon => self.cfg.icon_size = v.min(config::max_icon_size(self.cfg.height)),
            Slider::Tray => self.cfg.tray_icon_size = v.min(config::max_icon_size(self.cfg.height)),
        }
        self.live_resize = true;
        self.invalidate_all();
        if let Some(p) = &self.settings {
            p.invalidate();
        }
    }

    /// Saves the three size settings to config.toml and loads sharp icons again.
    fn commit_settings(&mut self) {
        for slider in settings::Slider::ALL {
            config::set_value(slider.key(), &format!("{}", slider.value(&self.cfg).round()));
        }
        self.cfg_mtime = config::mtime();
        self.live_resize = false;
        self.reposition();
        self.invalidate_all();
    }

    /// Moves the bar to another screen edge and saves it to config.toml.
    fn set_position(&mut self, pos: Position) {
        if self.cfg.position == pos {
            return;
        }
        config::set_value("position", &format!("\"{}\"", pos.name()));
        self.cfg_mtime = config::mtime();
        self.cfg.position = pos;
        self.rebuild_bars();
        self.rescan();
        if let Some(p) = &self.settings {
            p.invalidate();
        }
    }

    /// Moves the metrics to the other end of the bar and saves it to config.toml.
    fn set_monitor_side(&mut self, side: Side) {
        if self.cfg.monitor_side == side {
            return;
        }
        config::set_value("monitor_side", &format!("\"{}\"", side.name()));
        self.cfg_mtime = config::mtime();
        self.cfg.monitor_side = side;
        self.invalidate_all();
    }

    fn reset_settings(&mut self) {
        let d = Config::default();
        self.cfg.height = d.height;
        self.cfg.icon_size = d.icon_size;
        self.cfg.tray_icon_size = d.tray_icon_size;
        self.commit_settings();
        if let Some(p) = &self.settings {
            p.invalidate();
        }
    }

    fn hide_start(&mut self) {
        if let Some(m) = self.start.as_mut() {
            m.hide();
        }
    }

    fn hide_overflow(&mut self) {
        stop_watching_outside_clicks();
        if let Some(f) = self.flyout.as_mut() {
            if f.open {
                f.hide();
                self.invalidate_all();
            }
        }
    }

    /// Where tray icon `idx` is on screen: in the open flyout, on a bar (primary
    /// first), or else the ^ button that hides it.
    fn icon_screen_rect(&self, idx: usize) -> Option<RECT> {
        if let Some(r) = self.flyout.as_ref().and_then(|f| f.icon_screen_rect(idx)) {
            return Some(r);
        }
        let mut bars: Vec<&Bar> = self.bars.iter().collect();
        bars.sort_by_key(|b| !b.primary);
        let on_bar = |hit: Hit| {
            bars.iter().find_map(|b| {
                let (_, r) = b.items.iter().find(|(h, _)| *h == hit)?;
                Some(RECT {
                    left: b.rect.left + r.left as i32,
                    top: b.rect.top + r.top as i32,
                    right: b.rect.left + r.right as i32,
                    bottom: b.rect.top + r.bottom as i32,
                })
            })
        };
        on_bar(Hit::Tray(idx)).or_else(|| on_bar(Hit::Overflow))
    }

    /// Re-reads which tray icons Windows shows on the taskbar vs. in the overflow.
    fn refresh_tray_settings(&mut self) {
        if self.tray.refresh_settings() {
            self.invalidate_all();
        }
        if let Some(f) = self.flyout.as_mut() {
            f.relayout(&self.cfg, &self.tray);
        }
    }

    fn action(&mut self, bar: usize, hit: Option<Hit>, btn: Btn) -> Act {
        use Btn::*;
        if hit != Some(Hit::Start) {
            self.hide_start();
        }
        if hit != Some(Hit::Overflow) {
            self.hide_overflow();
        }
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
            // Not key presses: Windows drops injected keys while an elevated app is focused.
            (Some(Hit::Start), Left) => {
                self.toggle_start(bar);
                Act::None
            }
            // not Win+X: Windows would open its menu at its own (hidden) Start button
            (Some(Hit::Start), Right) => Act::StartButtonMenu,
            // The shortcuts toggle the panels; the URIs only open them (a second click
            // couldn't close them), so the URIs are just the fallback for elevated focus.
            (Some(Hit::Battery), Left) => {
                self.toggle_power(bar);
                Act::None
            }
            (Some(Hit::Quick), Left) => Act::KeysOr(
                vec![VK_LWIN, VK_A],
                "ms-actioncenter:controlcenter/&showFooter=true",
            ),
            (Some(Hit::Clock), Left) => Act::KeysOr(vec![VK_LWIN, VK_N], "ms-actioncenter:"),
            (Some(Hit::Monitor), Left) => Act::Launch("taskmgr.exe".into()),
            (Some(Hit::Monitor), Right) => Act::MonitorMenu,
            (Some(Hit::Lang), Left | Right) => Act::LangMenu,
            (Some(Hit::Desktop), Left) => {
                self.toggle_minimize_all();
                Act::None
            }
            (Some(Hit::Overflow), Left) => {
                self.toggle_overflow(bar);
                Act::None
            }
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
        winkey::uninstall();
        self.tray_hook = None;
        if let Some(p) = self.settings.take() {
            unsafe {
                let _ = DestroyWindow(p.hwnd);
            }
        }
        if let Some(m) = self.start.take() {
            unsafe {
                let _ = DestroyWindow(m.hwnd);
            }
        }
        if let Some(f) = self.flyout.take() {
            unsafe {
                let _ = DestroyWindow(f.hwnd);
            }
        }
        self.destroy_bars();
        if self.native_hidden {
            shell::restore_native();
            self.native_hidden = false;
        }
    }
}

// ---------------------------------------------------------------------------
// menus (run outside of the app borrow: TrackPopupMenu pumps messages)
// ---------------------------------------------------------------------------

/// Shows a menu at the cursor; items are (id, text, checked), id 0 = separator.
/// Returns the chosen id (0 = none).
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
        // open the menu away from the screen edge the bar sits on
        let pos = with_app(|a| a.cfg.position).unwrap_or(Position::Bottom);
        let align = match pos {
            Position::Bottom => TPM_BOTTOMALIGN | TPM_LEFTALIGN,
            Position::Top => TPM_TOPALIGN | TPM_LEFTALIGN,
            Position::Left => TPM_TOPALIGN | TPM_LEFTALIGN,
            Position::Right => TPM_TOPALIGN | TPM_RIGHTALIGN,
        };
        let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | align, pt.x, pt.y, None, owner, None);
        let _ = PostMessageW(Some(owner), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        cmd.0 as usize
    }
}

fn run_action(owner: HWND, act: Act) {
    match act {
        Act::None => {}
        Act::KeysOr(keys, uri) => {
            if util::foreground_elevated() {
                util::shell_open(uri, None);
            } else {
                util::send_keys(&keys);
            }
        }
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
            items.push((0, String::new(), false));
            items.push((9, EXIT_LABEL.into(), false));
            match popup(owner, &items) {
                9 => unsafe { PostQuitMessage(0) },
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
                        syscommand(*h, SC_CLOSE);
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
        Act::StartButtonMenu => start_button_menu(owner),
        Act::LangMenu => lang_menu(owner),
        Act::MonitorMenu => monitor_menu(owner),
        Act::BarMenu => {
            let auto = autostart_enabled();
            // (position and sizes live in Taskbar settings; files can still be edited by hand)
            let items = vec![
                (6, EXIT_LABEL.to_string(), false),
                (0, String::new(), false),
                (1, "Task Manager".to_string(), false),
                (8, "Taskbar settings...".to_string(), false),
                (7, "Choose which tray icons show...".to_string(), false),
                (4, "Start with Windows".to_string(), auto),
                (0, String::new(), false),
                (5, "Restart taskbar".to_string(), false),
            ];
            match popup(owner, &items) {
                1 => util::shell_open("taskmgr.exe", None),
                // Windows' page with the "Other system tray icons" on/off list
                7 => util::shell_open("ms-settings:taskbar", None),
                8 => {
                    with_app(|a| a.open_settings(owner));
                }
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
        Some(a.action(i, hit, btn))
    })
    .flatten();
    if let Some(act) = act {
        run_action(hwnd, act);
    }
}

fn press(hwnd: HWND, lp: LPARAM) {
    let (x, y) = lparam_xy(lp);
    let hit = with_app(|a| {
        let i = a.bar_index(hwnd)?;
        let hit = a.bars[i].hit_test(x, y);
        a.bars[i].pressed = hit;
        let tray_pid = match hit {
            Some(Hit::Tray(t)) => a.tray.owner_pid(t),
            _ => None,
        };
        Some((hit, tray_pid))
    })
    .flatten();
    if let Some((hit, tray_pid)) = hit {
        dismiss_popup(hwnd, hit, tray_pid);
    }
}

/// The bar never takes focus by itself (clicks on app buttons must see which app
/// was focused), so popups that close when they lose focus (a tray app's panel,
/// Windows' flyouts) stayed open when the bar was clicked. Like on Windows'
/// taskbar, a click on the bar now takes focus from such a popup: it closes.
/// Normal app windows keep focus, and a click on the popup's own tray icon or on
/// a button that toggles a Windows flyout is left to toggle it.
fn dismiss_popup(bar: HWND, hit: Option<Hit>, tray_pid: Option<u32>) {
    if matches!(hit, Some(Hit::Quick | Hit::Battery | Hit::Clock)) {
        return;
    }
    let fg = unsafe { GetForegroundWindow() };
    if fg.is_invalid() || apps::is_taskbar_window(fg) {
        return;
    }
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(fg, Some(&mut pid)) };
    if pid == 0 || pid == std::process::id() || tray_pid == Some(pid) {
        return;
    }
    util::force_foreground(bar);
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

/// Tray action for a mouse button, with the flyout's pressed/hit check.
fn flyout_click(hwnd: HWND, lp: LPARAM, action: TrayAction) {
    let (x, y) = lparam_xy(lp);
    with_app(|a| {
        let Some(f) = a.flyout.as_mut() else { return };
        let hit = f.hit_test(x, y);
        let pressed = f.pressed.take();
        if action != TrayAction::Double && pressed != hit {
            return;
        }
        if let Some(i) = hit {
            // the app may now open a menu/popup and take focus: keep the flyout open
            f.interacting_pid = a.tray.owner_pid(i);
            a.tray.send(i, action);
        }
    });
    let _ = hwnd;
}

// After a click on a flyout icon, the icon's app usually shows a menu or popup and
// takes focus (sometimes from another process, e.g. Steam's menu). Like Windows'
// overflow, the flyout then stays open until the user clicks outside of it. A
// low-level mouse hook watches for that click only while in this state.

static FLYOUT_HWND: AtomicIsize = AtomicIsize::new(0);
static OUTSIDE_CLICK_HOOK: AtomicIsize = AtomicIsize::new(0);

unsafe extern "system" fn outside_click_proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if code >= 0 && matches!(wp.0 as u32, WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN) {
            if let Some(info) = (lp.0 as *const MSLLHOOKSTRUCT).as_ref() {
                let flyout = HWND(FLYOUT_HWND.load(Ordering::Relaxed) as *mut _);
                let mut r = RECT::default();
                let _ = GetWindowRect(flyout, &mut r);
                let p = info.pt;
                let inside = p.x >= r.left && p.x < r.right && p.y >= r.top && p.y < r.bottom;
                if !inside {
                    let _ = PostMessageW(Some(flyout), WM_FLYOUT_CLOSE, WPARAM(0), LPARAM(0));
                }
            }
        }
        CallNextHookEx(None, code, wp, lp)
    }
}

fn watch_outside_clicks(flyout: HWND) {
    FLYOUT_HWND.store(flyout.0 as isize, Ordering::Relaxed);
    if OUTSIDE_CLICK_HOOK.load(Ordering::Relaxed) != 0 {
        return;
    }
    let hinst = unsafe { GetModuleHandleW(None) }.unwrap_or_default();
    if let Ok(h) = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(outside_click_proc), Some(hinst.into()), 0) } {
        OUTSIDE_CLICK_HOOK.store(h.0 as isize, Ordering::Relaxed);
    }
}

fn stop_watching_outside_clicks() {
    let h = OUTSIDE_CLICK_HOOK.swap(0, Ordering::Relaxed);
    if h != 0 {
        unsafe {
            let _ = UnhookWindowsHookEx(HHOOK(h as *mut _));
        }
    }
}

unsafe extern "system" fn flyout_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                BeginPaint(hwnd, &mut ps);
                let _ = EndPaint(hwnd, &ps);
                let done = with_app(|a| {
                    if let Some(f) = a.flyout.as_mut() {
                        f.paint(&a.d2d, &a.cfg, &a.tray);
                    }
                });
                if done.is_none() {
                    let _ = PostMessageW(Some(hwnd), WM_REPAINT, WPARAM(0), LPARAM(0));
                }
                LRESULT(0)
            }
            WM_REPAINT => {
                let _ = InvalidateRect(Some(hwnd), None, false);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_ACTIVATE => {
                // lost focus (clicked elsewhere, or a tray app opened its window/menu)
                if (wp.0 & 0xFFFF) as u32 == WA_INACTIVE {
                    let _ = PostMessageW(Some(hwnd), WM_FLYOUT_HIDE, WPARAM(0), LPARAM(0));
                }
                LRESULT(0)
            }
            WM_FLYOUT_HIDE => {
                // lost focus: close, unless an icon was just clicked (its app's menu
                // took focus) - then close on the next click outside instead
                let done = with_app(|a| {
                    let interacting = a.flyout.as_ref().is_some_and(|f| f.open && f.interacting_pid.is_some());
                    if interacting {
                        watch_outside_clicks(hwnd);
                    } else {
                        a.hide_overflow();
                    }
                });
                if done.is_none() {
                    let _ = PostMessageW(Some(hwnd), WM_FLYOUT_HIDE, WPARAM(0), LPARAM(0));
                }
                LRESULT(0)
            }
            WM_FLYOUT_CLOSE => {
                if with_app(|a| a.hide_overflow()).is_none() {
                    let _ = PostMessageW(Some(hwnd), WM_FLYOUT_CLOSE, WPARAM(0), LPARAM(0));
                }
                LRESULT(0)
            }
            WM_KEYDOWN if wp.0 == 0x1B => {
                // Esc
                with_app(|a| a.hide_overflow());
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                let (x, y) = lparam_xy(lp);
                with_app(|a| {
                    let Some(f) = a.flyout.as_mut() else { return };
                    if !f.tracking {
                        let mut tme = TRACKMOUSEEVENT {
                            cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                            dwFlags: TME_LEAVE,
                            hwndTrack: hwnd,
                            dwHoverTime: 0,
                        };
                        let _ = TrackMouseEvent(&mut tme);
                        f.tracking = true;
                    }
                    let hit = f.hit_test(x, y);
                    if hit != f.hover {
                        if let Some(t) = f.hover {
                            a.tray.send(t, TrayAction::HoverLeave);
                        }
                        if let Some(t) = hit {
                            a.tray.send(t, TrayAction::HoverEnter);
                        }
                        f.hover = hit;
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                    if let Some(t) = hit {
                        a.tray.send(t, TrayAction::Move);
                    }
                });
                LRESULT(0)
            }
            WM_MOUSELEAVE_ => {
                with_app(|a| {
                    let Some(f) = a.flyout.as_mut() else { return };
                    if let Some(t) = f.hover.take() {
                        a.tray.send(t, TrayAction::HoverLeave);
                    }
                    f.tracking = false;
                    f.pressed = None;
                    let _ = InvalidateRect(Some(hwnd), None, false);
                });
                LRESULT(0)
            }
            WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN => {
                let (x, y) = lparam_xy(lp);
                with_app(|a| {
                    if let Some(f) = a.flyout.as_mut() {
                        f.pressed = f.hit_test(x, y);
                    }
                });
                LRESULT(0)
            }
            WM_LBUTTONDBLCLK => {
                with_app(|a| {
                    if let Some(f) = a.flyout.as_mut() {
                        f.swallow_up = true;
                    }
                });
                flyout_click(hwnd, lp, TrayAction::Double);
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                let swallow = with_app(|a| a.flyout.as_mut().map(|f| std::mem::take(&mut f.swallow_up)))
                    .flatten()
                    .unwrap_or(false);
                if !swallow {
                    flyout_click(hwnd, lp, TrayAction::Left);
                }
                LRESULT(0)
            }
            WM_RBUTTONUP => {
                flyout_click(hwnd, lp, TrayAction::Right);
                LRESULT(0)
            }
            WM_MBUTTONUP => {
                flyout_click(hwnd, lp, TrayAction::Middle);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

/// Launches what a Start panel hit points to and closes the panel.
fn start_launch(hit: Option<startmenu::StartHit>) {
    start_launch_as(hit, false);
}

fn start_launch_as(hit: Option<startmenu::StartHit>, admin: bool) {
    let target = with_app(|a| {
        let m = a.start.as_mut()?;
        let h = hit?;
        let t = m.launch_target(h)?;
        let admin = admin && m.can_elevate(h);
        m.hide();
        Some((t, admin))
    })
    .flatten();
    if let Some(((file, args), admin)) = target {
        if admin {
            util::shell_open_admin(&file, args.as_deref());
        } else {
            util::shell_open(&file, args.as_deref());
        }
    }
}

/// The Start button's right-click menu, with the entries of Windows' Win+X menu.
fn start_button_menu(owner: HWND) {
    use power::Power;
    enum Item {
        Open(&'static str),
        Admin(&'static str),
        Keys(Vec<VIRTUAL_KEY>),
        Power(Power),
        Desktop,
    }
    let entries: Vec<Option<(&str, Item)>> = vec![
        Some(("Installed apps", Item::Open("ms-settings:appsfeatures"))),
        Some(("Power Options", Item::Open("ms-settings:powersleep"))),
        Some(("Event Viewer", Item::Open("eventvwr.msc"))),
        Some(("System", Item::Open("ms-settings:about"))),
        Some(("Device Manager", Item::Open("devmgmt.msc"))),
        Some(("Network Connections", Item::Open("ms-settings:network"))),
        Some(("Disk Management", Item::Admin("diskmgmt.msc"))),
        Some(("Computer Management", Item::Admin("compmgmt.msc"))),
        Some(("Terminal", Item::Open("wt.exe"))),
        Some(("Terminal (Admin)", Item::Admin("wt.exe"))),
        None,
        Some(("Task Manager", Item::Open("taskmgr.exe"))),
        Some(("Settings", Item::Open("ms-settings:"))),
        Some(("File Explorer", Item::Open("explorer.exe"))),
        Some(("Search", Item::Keys(vec![VK_LWIN, VK_S]))),
        Some(("Run", Item::Keys(vec![VK_LWIN, VK_R]))),
        None,
        Some(("Turn off screen", Item::Power(Power::ScreenOff))),
        Some(("Lock", Item::Power(Power::Lock))),
        Some(("Sign out", Item::Power(Power::SignOut))),
        Some(("Sleep", Item::Power(Power::Sleep))),
        Some(("Shut down", Item::Power(Power::Shutdown))),
        Some(("Restart", Item::Power(Power::Restart))),
        None,
        Some(("Desktop", Item::Desktop)),
    ];
    let items: Vec<(usize, String, bool)> = entries
        .iter()
        .enumerate()
        .map(|(i, e)| match e {
            Some((label, _)) => (i + 1, label.to_string(), false),
            None => (0, String::new(), false),
        })
        .collect();
    let chosen = popup(owner, &items);
    let Some(Some((_, item))) = chosen.checked_sub(1).and_then(|i| entries.get(i)) else { return };
    match item {
        Item::Open(t) => util::shell_open(t, None),
        Item::Admin(t) => util::shell_open_admin(t, None),
        Item::Keys(k) => util::send_keys(k),
        Item::Power(p) => p.run(owner),
        Item::Desktop => {
            with_app(|a| a.toggle_minimize_all());
        }
    }
}

/// Right-click menu of the metrics: put them at either end of the bar.
fn monitor_menu(owner: HWND) {
    let Some((side, vertical)) = with_app(|a| (a.cfg.monitor_side, a.cfg.position.is_vertical())) else { return };
    let (near, far) = if vertical {
        ("Place at the top", "Place at the bottom (by the clock)")
    } else {
        ("Place on the left", "Place on the right (by the clock)")
    };
    let items = vec![
        (1, near.to_string(), side == Side::Start),
        (2, far.to_string(), side == Side::End),
        (0, String::new(), false),
        (3, "Task Manager".to_string(), false),
    ];
    match popup(owner, &items) {
        1 => {
            with_app(|a| a.set_monitor_side(Side::Start));
        }
        2 => {
            with_app(|a| a.set_monitor_side(Side::End));
        }
        3 => util::shell_open("taskmgr.exe", None),
        _ => {}
    }
}

/// Installed input languages; picking one switches the window that had focus.
fn lang_menu(owner: HWND) {
    let Some((target, current)) = with_app(|a| (a.lang_target, a.lang.as_ref().map(|l| l.hkl))) else { return };
    let langs = lang::installed();
    let mut items: Vec<(usize, String, bool)> =
        langs.iter().enumerate().map(|(i, l)| (i + 1, l.menu_label(), Some(l.hkl) == current)).collect();
    items.push((0, String::new(), false));
    items.push((1000, "Language settings".into(), false));
    let chosen = popup(owner, &items);
    if chosen == 1000 {
        util::shell_open("ms-settings:regionlanguage", None);
        return;
    }
    let Some(l) = chosen.checked_sub(1).and_then(|i| langs.get(i)) else { return };
    if !target.is_invalid() && unsafe { IsWindow(Some(target)) }.as_bool() {
        util::force_foreground(target);
        lang::activate(target, l.hkl);
    }
    with_app(|a| {
        a.lang = Some(l.clone());
        a.invalidate_all();
    });
}

/// Account and power menus of the Start panel's footer buttons.
fn start_footer_menu(hwnd: HWND, hit: startmenu::StartHit) {
    use power::Power;
    let choices: &[Power] = match hit {
        startmenu::StartHit::User => &[Power::Lock, Power::SignOut],
        startmenu::StartHit::Power => &[Power::ScreenOff, Power::Sleep, Power::Shutdown, Power::Restart],
        _ => return,
    };
    let items: Vec<(usize, String, bool)> =
        choices.iter().enumerate().map(|(i, p)| (i + 1, p.label().to_string(), false)).collect();
    let chosen = popup(hwnd, &items);
    if let Some(p) = chosen.checked_sub(1).and_then(|i| choices.get(i)) {
        with_app(|a| a.hide_start());
        p.run(hwnd);
    }
}

/// Right-click menu of a Start panel entry: pin/unpin, open.
fn start_context_menu(hwnd: HWND, hit: startmenu::StartHit) {
    let Some((id, pinned, elevate)) = with_app(|a| {
        let m = a.start.as_ref()?;
        let id = m.id_of(hit);
        let pinned = id.as_ref().is_some_and(|id| m.is_pinned(id));
        Some((id, pinned, m.can_elevate(hit)))
    })
    .flatten() else {
        return;
    };
    let mut items = vec![(1, "Open".to_string(), false)];
    if elevate {
        items.push((3, "Run as administrator".to_string(), false));
    }
    if id.is_some() {
        items.push((2, if pinned { "Unpin from Start" } else { "Pin to Start" }.to_string(), false));
    }
    match popup(hwnd, &items) {
        1 => start_launch(Some(hit)),
        3 => start_launch_as(Some(hit), true),
        2 => {
            let Some(id) = id else { return };
            with_app(|a| {
                if let Some(m) = a.start.as_mut() {
                    m.toggle_pin(&id);
                }
            });
        }
        _ => {}
    }
}

unsafe extern "system" fn settings_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    use settings::SettingsHit;
    unsafe {
        match msg {
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                BeginPaint(hwnd, &mut ps);
                let _ = EndPaint(hwnd, &ps);
                let done = with_app(|a| {
                    if let Some(p) = a.settings.as_mut() {
                        p.paint(&a.d2d, &a.dw, &a.cfg);
                    }
                });
                if done.is_none() {
                    let _ = PostMessageW(Some(hwnd), WM_REPAINT, WPARAM(0), LPARAM(0));
                }
                LRESULT(0)
            }
            WM_REPAINT => {
                let _ = InvalidateRect(Some(hwnd), None, false);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_ACTIVATE => {
                if (wp.0 & 0xFFFF) as u32 == WA_INACTIVE {
                    let _ = PostMessageW(Some(hwnd), WM_SETTINGS_HIDE, WPARAM(0), LPARAM(0));
                }
                LRESULT(0)
            }
            WM_SETTINGS_HIDE => {
                let done = with_app(|a| {
                    // don't close mid-drag (the mouse is captured)
                    if let Some(p) = a.settings.as_mut() {
                        if p.dragging.is_none() {
                            p.hide();
                        }
                    }
                });
                if done.is_none() {
                    let _ = PostMessageW(Some(hwnd), WM_SETTINGS_HIDE, WPARAM(0), LPARAM(0));
                }
                LRESULT(0)
            }
            WM_KEYDOWN if wp.0 == 0x1B => {
                with_app(|a| a.settings.as_mut().map(|p| p.hide()));
                LRESULT(0)
            }
            WM_LBUTTONDOWN => {
                let (x, y) = lparam_xy(lp);
                with_app(|a| {
                    let Some(p) = a.settings.as_mut() else { return };
                    let hit = p.hit_test(x, y);
                    match hit {
                        Some(SettingsHit::Slider(s)) => {
                            p.dragging = Some(s);
                            let value = p.value_at(s, x);
                            let _ = windows::Win32::UI::Input::KeyboardAndMouse::SetCapture(hwnd);
                            if let Some(v) = value {
                                a.set_slider(s, v);
                            }
                        }
                        Some(SettingsHit::Reset) => a.reset_settings(),
                        Some(SettingsHit::Position(pos)) => a.set_position(pos),
                        Some(SettingsHit::Close) => p.hide(),
                        None => {}
                    }
                });
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                let (x, y) = lparam_xy(lp);
                with_app(|a| {
                    let Some(p) = a.settings.as_mut() else { return };
                    if !p.tracking {
                        let mut tme = TRACKMOUSEEVENT {
                            cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                            dwFlags: TME_LEAVE,
                            hwndTrack: hwnd,
                            dwHoverTime: 0,
                        };
                        let _ = TrackMouseEvent(&mut tme);
                        p.tracking = true;
                    }
                    if let Some(s) = p.dragging {
                        let value = p.value_at(s, x);
                        if let Some(v) = value {
                            a.set_slider(s, v);
                        }
                        return;
                    }
                    let hit = p.hit_test(x, y);
                    if hit != p.hover {
                        p.hover = hit;
                        p.invalidate();
                    }
                });
                LRESULT(0)
            }
            WM_MOUSELEAVE_ => {
                with_app(|a| {
                    if let Some(p) = a.settings.as_mut() {
                        p.tracking = false;
                        if p.dragging.is_none() {
                            p.hover = None;
                            p.invalidate();
                        }
                    }
                });
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                let _ = windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture();
                with_app(|a| {
                    let was_dragging = a.settings.as_mut().and_then(|p| p.dragging.take()).is_some();
                    if was_dragging {
                        a.commit_settings();
                        if let Some(p) = &a.settings {
                            p.invalidate();
                        }
                    }
                });
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

unsafe extern "system" fn power_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                BeginPaint(hwnd, &mut ps);
                let _ = EndPaint(hwnd, &ps);
                let done = with_app(|a| {
                    if let Some(p) = a.power.as_mut() {
                        p.paint(&a.d2d, &a.dw, &a.cfg);
                    }
                });
                if done.is_none() {
                    let _ = PostMessageW(Some(hwnd), WM_REPAINT, WPARAM(0), LPARAM(0));
                }
                LRESULT(0)
            }
            WM_REPAINT => {
                let _ = InvalidateRect(Some(hwnd), None, false);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_ACTIVATE => {
                if (wp.0 & 0xFFFF) as u32 == WA_INACTIVE {
                    let _ = PostMessageW(Some(hwnd), WM_POWER_HIDE, WPARAM(0), LPARAM(0));
                }
                LRESULT(0)
            }
            WM_POWER_HIDE => {
                if with_app(|a| a.power.as_mut().map(|p| p.hide())).is_none() {
                    let _ = PostMessageW(Some(hwnd), WM_POWER_HIDE, WPARAM(0), LPARAM(0));
                }
                LRESULT(0)
            }
            WM_KEYDOWN if wp.0 == 0x1B => {
                with_app(|a| a.power.as_mut().map(|p| p.hide()));
                LRESULT(0)
            }
            WM_LBUTTONDOWN => {
                let (x, y) = lparam_xy(lp);
                with_app(|a| {
                    let Some(p) = a.power.as_mut() else { return };
                    if let Some(hit) = p.hit_test(x, y) {
                        if p.click(hit) {
                            p.hide();
                        }
                    }
                });
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                let (x, y) = lparam_xy(lp);
                with_app(|a| {
                    let Some(p) = a.power.as_mut() else { return };
                    if !p.tracking {
                        let mut tme = TRACKMOUSEEVENT {
                            cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                            dwFlags: TME_LEAVE,
                            hwndTrack: hwnd,
                            dwHoverTime: 0,
                        };
                        let _ = TrackMouseEvent(&mut tme);
                        p.tracking = true;
                    }
                    let hit = p.hit_test(x, y);
                    if hit != p.hover {
                        p.hover = hit;
                        p.invalidate();
                    }
                });
                LRESULT(0)
            }
            WM_MOUSELEAVE_ => {
                with_app(|a| {
                    if let Some(p) = a.power.as_mut() {
                        p.tracking = false;
                        p.hover = None;
                        p.invalidate();
                    }
                });
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

unsafe extern "system" fn start_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                BeginPaint(hwnd, &mut ps);
                let _ = EndPaint(hwnd, &ps);
                let done = with_app(|a| {
                    if let Some(m) = a.start.as_mut() {
                        if let Some(loader) = a.icon_loader.as_mut() {
                            m.paint(&a.d2d, &a.dw, &a.cfg, &mut a.icons, loader);
                        }
                    }
                });
                if done.is_none() {
                    let _ = PostMessageW(Some(hwnd), WM_REPAINT, WPARAM(0), LPARAM(0));
                }
                LRESULT(0)
            }
            WM_REPAINT => {
                let _ = InvalidateRect(Some(hwnd), None, false);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_ACTIVATE => {
                // clicked elsewhere: close (our own right-click menu keeps it active)
                if (wp.0 & 0xFFFF) as u32 == WA_INACTIVE {
                    let _ = PostMessageW(Some(hwnd), WM_START_HIDE, WPARAM(0), LPARAM(0));
                }
                LRESULT(0)
            }
            WM_START_HIDE => {
                if with_app(|a| a.hide_start()).is_none() {
                    let _ = PostMessageW(Some(hwnd), WM_START_HIDE, WPARAM(0), LPARAM(0));
                }
                LRESULT(0)
            }
            WM_KEYDOWN if wp.0 == 0x1B => {
                with_app(|a| a.hide_start());
                LRESULT(0)
            }
            WM_CHAR => {
                let c = wp.0 as u32;
                match c {
                    0x08 => {
                        with_app(|a| a.start.as_mut().map(|m| m.backspace()));
                    }
                    0x0D => {
                        // Enter: open the top match; Ctrl+Shift+Enter as administrator
                        let first = with_app(|a| a.start.as_ref().and_then(|m| m.filtered().first().copied())).flatten();
                        use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_CONTROL, VK_SHIFT};
                        let down = |vk: VIRTUAL_KEY| GetKeyState(vk.0 as i32) < 0;
                        start_launch_as(first, down(VK_CONTROL) && down(VK_SHIFT));
                    }
                    0x09 => {
                        with_app(|a| a.start.as_mut().map(|m| m.complete()));
                    }
                    0x1B => {}
                    _ => {
                        if let Some(ch) = char::from_u32(c).filter(|ch| !ch.is_control()) {
                            with_app(|a| a.start.as_mut().map(|m| m.type_char(ch)));
                        }
                    }
                }
                LRESULT(0)
            }
            WM_MOUSEWHEEL => {
                let delta = ((wp.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
                with_app(|a| a.start.as_mut().map(|m| m.scroll_by(delta)));
                // hover follows the content under the cursor
                let _ = PostMessageW(Some(hwnd), WM_MOUSEMOVE, WPARAM(0), {
                    let mut p = POINT::default();
                    let _ = GetCursorPos(&mut p);
                    let _ = windows::Win32::Graphics::Gdi::ScreenToClient(hwnd, &mut p);
                    LPARAM(((p.x as u16 as u32) | ((p.y as u16 as u32) << 16)) as isize)
                });
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                let (x, y) = lparam_xy(lp);
                with_app(|a| {
                    let Some(m) = a.start.as_mut() else { return };
                    if !m.tracking {
                        let mut tme = TRACKMOUSEEVENT {
                            cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                            dwFlags: TME_LEAVE,
                            hwndTrack: hwnd,
                            dwHoverTime: 0,
                        };
                        let _ = TrackMouseEvent(&mut tme);
                        m.tracking = true;
                    }
                    let hit = m.hit_test(x, y);
                    if hit != m.hover {
                        m.hover = hit;
                        m.invalidate();
                    }
                });
                LRESULT(0)
            }
            WM_MOUSELEAVE_ => {
                with_app(|a| {
                    if let Some(m) = a.start.as_mut() {
                        m.tracking = false;
                        m.hover = None;
                        m.invalidate();
                    }
                });
                LRESULT(0)
            }
            WM_LBUTTONDOWN | WM_RBUTTONDOWN => {
                let (x, y) = lparam_xy(lp);
                with_app(|a| {
                    if let Some(m) = a.start.as_mut() {
                        m.pressed = m.hit_test(x, y);
                    }
                });
                LRESULT(0)
            }
            WM_LBUTTONUP | WM_RBUTTONUP => {
                let (x, y) = lparam_xy(lp);
                let hit = with_app(|a| {
                    let m = a.start.as_mut()?;
                    let hit = m.hit_test(x, y);
                    (m.pressed.take() == hit).then_some(hit).flatten()
                })
                .flatten();
                if let Some(hit) = hit {
                    if matches!(hit, startmenu::StartHit::User | startmenu::StartHit::Power) {
                        start_footer_menu(hwnd, hit);
                    } else if msg == WM_LBUTTONUP {
                        start_launch(Some(hit));
                    } else {
                        start_context_menu(hwnd, hit);
                    }
                }
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

unsafe extern "system" fn receiver_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_WIN_START => {
                with_app(|a| a.open_start_from_key());
                return LRESULT(0);
            }
            WM_APPS_READY => {
                let apps = startmenu::ENUMERATED.lock().ok().and_then(|mut s| s.take());
                if let Some((apps, commands)) = apps {
                    with_app(|a| {
                        if let (Some(m), Some(loader)) = (a.start.as_mut(), a.icon_loader.as_mut()) {
                            m.set_apps(apps, commands);
                            m.preload_icons(&a.icons, loader);
                        }
                    });
                }
                return LRESULT(0);
            }
            WM_ICONS_READY => {
                let done = with_app(|a| {
                    let Some(loader) = a.icon_loader.as_mut() else { return };
                    if loader.take_loaded(&mut a.icons) {
                        if let Some(m) = a.start.as_ref().filter(|m| m.open) {
                            m.invalidate();
                        }
                    }
                });
                if done.is_none() {
                    let _ = PostMessageW(Some(hwnd), WM_ICONS_READY, WPARAM(0), LPARAM(0));
                }
                return LRESULT(0);
            }
            WM_COPYDATA => {
                let Some(cds) = (lp.0 as *const COPYDATASTRUCT).as_ref() else {
                    return LRESULT(0);
                };
                if cds.lpData.is_null() {
                    return LRESULT(0);
                }
                let bytes = std::slice::from_raw_parts(cds.lpData as *const u8, cds.cbData as usize);
                if cds.dwData == tray::RECT_TAG {
                    // an app asked where its tray icon is (Shell_NotifyIconGetRect), to place
                    // its menu/flyout. Reply: one corner packed as two 16-bit coordinates.
                    let reply = with_app(|a| {
                        let (idx, bottom_right) = a.tray.identify(bytes)?;
                        let r = a.icon_screen_rect(idx)?;
                        let (x, y) = if bottom_right { (r.right, r.bottom) } else { (r.left, r.top) };
                        Some(((x as u32 & 0xFFFF) | ((y as u32 & 0xFFFF) << 16)) as i32 as isize)
                    })
                    .flatten()
                    .unwrap_or(0);
                    return LRESULT(reply);
                }
                if cds.dwData != tray::FORWARD_TAG {
                    return LRESULT(0);
                }
                let added = with_app(|a| {
                    let before = a.tray.icons.len();
                    if a.tray.handle(bytes) {
                        a.invalidate_all();
                        if let Some(f) = a.flyout.as_mut() {
                            f.relayout(&a.cfg, &a.tray);
                        }
                    }
                    a.tray.icons.len() != before
                })
                .unwrap_or(false);
                if added {
                    // explorer writes the icon's settings entry after handling NIM_ADD
                    schedule(T_TRAY_SETTINGS, 800);
                }
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
                    T_LANG => {
                        with_app(|a| a.update_lang());
                    }
                    T_MONITOR => {
                        with_app(|a| {
                            let st = a.monitor.sample();
                            if st != a.stats {
                                a.stats = st;
                                if a.cfg.show_monitor {
                                    a.invalidate_all();
                                }
                            }
                        });
                    }
                    T_LIVE_RESIZE => {
                        let _ = KillTimer(Some(hwnd), T_LIVE_RESIZE);
                        LIVE_RESIZE_PENDING.store(false, Ordering::Release);
                        with_app(|a| a.reposition());
                    }
                    T_TRAY_SETTINGS => {
                        let _ = KillTimer(Some(hwnd), T_TRAY_SETTINGS);
                        with_app(|a| a.refresh_tray_settings());
                    }
                    T_MAINT => {
                        with_app(|a| {
                            if a.tray.prune() {
                                a.invalidate_all();
                            }
                            // picks up changes made in Windows' tray icon settings
                            a.refresh_tray_settings();
                            if a.native_hidden {
                                shell::enforce_hidden();
                                if shell::enforce_autohide() {
                                    log!("native taskbar lost auto-hide; set again");
                                    schedule(T_REPOSITION, 300);
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
                    // explorer restarted (ignore the echo of our own broadcast, unless the
                    // taskbar window is a new one: explorer's broadcast can land within
                    // the same few seconds right after sign-in)
                    let restarted = with_app(|a| {
                        a.last_broadcast.elapsed() >= Duration::from_secs(3) || primary_tray() != a.tray_wnd
                    })
                    .unwrap_or(false);
                    if restarted {
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

fn primary_tray() -> isize {
    shell::native_taskbars()
        .into_iter()
        .find(|&h| util::window_class(h) == "Shell_TrayWnd")
        .map(|h| h.0 as isize)
        .unwrap_or(0)
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
            // the app icon (resource 1, docksy.rc) for the settings window, Alt+Tab
            hIcon: LoadIconW(Some(hinst.into()), PCWSTR(1 as _)).unwrap_or_default(),
            lpszClassName: name,
            ..Default::default()
        };
        RegisterClassExW(&wc);
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--quit") {
        quit_running_instance();
        return;
    }
    if args.iter().any(|a| a == "--restore") {
        shell::restore_native();
        return;
    }
    if args.iter().any(|a| a == "--dump") {
        // diagnostics: how pins resolve and what windows we see, written to docksy.log
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        }
        for p in pins::load() {
            log!("pin {:?}", p);
        }
        for w in WindowTracker::default().scan() {
            log!("window {:?} exe={} aumid={:?} title={}", w.hwnd.0, w.exe, w.aumid, w.title);
        }
        for a in startmenu::enumerate_apps() {
            log!("start app {} | {}", a.name, a.id);
        }
        return;
    }

    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _mutex = CreateMutexW(None, true, w!("Local\\DocksyTaskbar"));
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
    backdrop::use_dark_menus();

    std::panic::set_hook(Box::new(|info| {
        log!("panic: {info}");
        shell::restore_native();
    }));
    log!("starting");

    register_class(w!("DocksyTrayReceiver"), Some(receiver_proc), false);
    register_class(w!("DocksyBar"), Some(bar_proc), true);
    register_class(w!("DocksyFlyout"), Some(flyout_proc), true);
    register_class(w!("DocksyStart"), Some(start_proc), true);
    register_class(w!("DocksySettings"), Some(settings_proc), false);
    register_class(w!("DocksyPower"), Some(power_proc), false);

    let recv = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
            w!("DocksyTrayReceiver"),
            w!("Docksy"),
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
        a.flyout = flyout::Flyout::new(unsafe { GetModuleHandleW(None) }.unwrap_or_default().into());
        a.start = startmenu::StartMenu::new(unsafe { GetModuleHandleW(None) }.unwrap_or_default().into());
        a.icon_loader = Some(icons::IconLoader::new(receiver(), WM_ICONS_READY));
        // build the app list and its icons now, so the first Start opens ready
        let scale = a.bars.first().map(|b| b.scale).unwrap_or(1.0);
        if let Some(m) = a.start.as_mut() {
            m.set_scale(scale);
            startmenu::refresh_apps_async(receiver(), WM_APPS_READY);
        }
        a.settings = settings::SettingsPanel::new(unsafe { GetModuleHandleW(None) }.unwrap_or_default().into());
        a.power = powerpanel::PowerPanel::new(unsafe { GetModuleHandleW(None) }.unwrap_or_default().into());
    });

    let flags = WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS;
    let ranges = [
        (EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND),
        (EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MINIMIZEEND),
        (EVENT_OBJECT_CREATE, EVENT_OBJECT_HIDE),
        (EVENT_OBJECT_NAMECHANGE, EVENT_OBJECT_NAMECHANGE),
        (EVENT_OBJECT_CLOAKED, EVENT_OBJECT_UNCLOAKED),
    ];
    winkey::install(recv, WM_WIN_START);
    with_app(|a| winkey::set_enabled(a.cfg.win_key_opens_start));

    let _hooks: Vec<HWINEVENTHOOK> = ranges
        .iter()
        .map(|&(lo, hi)| unsafe { SetWinEventHook(lo, hi, None, Some(win_event), 0, 0, flags) })
        .collect();

    schedule(T_CLOCK, 1000);
    schedule(T_MONITOR, 2000);
    schedule(T_LANG, 250);
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
