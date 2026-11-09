//! Window background: the Windows-taskbar "glass" (acrylic blur of what's behind,
//! tinted), Mica, or a plain color.
//!
//! DWM's documented backdrop (DWMWA_SYSTEMBACKDROP_TYPE) doesn't apply to
//! borderless popup windows like our bar, so acrylic uses the accent policy of
//! `SetWindowCompositionAttribute` (undocumented, user32) - what the Windows
//! taskbar itself and tools like TranslucentTB use.

use std::ffi::c_void;

use windows::Win32::{
    Foundation::HWND,
    Graphics::{
        Direct2D::Common::D2D1_COLOR_F,
        Dwm::{
            DWM_SYSTEMBACKDROP_TYPE, DWM_WINDOW_CORNER_PREFERENCE, DWMSBT_MAINWINDOW, DWMSBT_NONE,
            DWMWA_SYSTEMBACKDROP_TYPE, DWMWA_USE_IMMERSIVE_DARK_MODE,
            DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND, DWMWCP_ROUND,
            DwmExtendFrameIntoClientArea, DwmSetWindowAttribute,
        },
    },
    System::LibraryLoader::{GetModuleHandleW, GetProcAddress},
    UI::Controls::MARGINS,
};
use windows::core::{BOOL, s, w};

use crate::{
    config::{Backdrop, Config},
    util::parse_color,
};

const WCA_ACCENT_POLICY: u32 = 19;
const ACCENT_DISABLED: u32 = 0;
const ACCENT_ENABLE_ACRYLICBLURBEHIND: u32 = 4;

#[repr(C)]
struct AccentPolicy {
    state: u32,
    flags: u32,
    /// tint, 0xAABBGGRR
    gradient_color: u32,
    animation_id: u32,
}

#[repr(C)]
struct WindowCompositionAttribData {
    attribute: u32,
    data: *mut c_void,
    size: usize,
}

type SetWindowCompositionAttribute =
    unsafe extern "system" fn(HWND, *mut WindowCompositionAttribData) -> BOOL;

fn set_accent(hwnd: HWND, state: u32, tint: D2D1_COLOR_F) -> bool {
    unsafe {
        let Ok(user32) = GetModuleHandleW(w!("user32.dll")) else { return false };
        let Some(f) = GetProcAddress(user32, s!("SetWindowCompositionAttribute")) else {
            return false;
        };
        let f: SetWindowCompositionAttribute = std::mem::transmute(f);
        let byte = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u32;
        let mut policy = AccentPolicy {
            state,
            flags: 0,
            gradient_color: byte(tint.a) << 24 | byte(tint.b) << 16 | byte(tint.g) << 8 | byte(tint.r),
            animation_id: 0,
        };
        let mut data = WindowCompositionAttribData {
            attribute: WCA_ACCENT_POLICY,
            data: &mut policy as *mut _ as *mut c_void,
            size: std::mem::size_of::<AccentPolicy>(),
        };
        f(hwnd, &mut data).as_bool()
    }
}

fn set_attr<T>(hwnd: HWND, attr: windows::Win32::Graphics::Dwm::DWMWINDOWATTRIBUTE, value: &T) {
    unsafe {
        let _ = DwmSetWindowAttribute(hwnd, attr, value as *const T as *const c_void, std::mem::size_of::<T>() as u32);
    }
}

/// Applies the configured background to a bar (`rounded` = false) or flyout.
pub fn apply(hwnd: HWND, cfg: &Config, rounded: bool) {
    let corner: DWM_WINDOW_CORNER_PREFERENCE = if rounded { DWMWCP_ROUND } else { DWMWCP_DONOTROUND };
    set_attr(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &corner);
    set_attr(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &BOOL(1));

    let acrylic = cfg.backdrop == Backdrop::Acrylic
        && set_accent(hwnd, ACCENT_ENABLE_ACRYLICBLURBEHIND, parse_color(&cfg.background));
    if !acrylic {
        set_accent(hwnd, ACCENT_DISABLED, parse_color(&cfg.background));
    }
    let kind: DWM_SYSTEMBACKDROP_TYPE = if cfg.backdrop == Backdrop::Mica { DWMSBT_MAINWINDOW } else { DWMSBT_NONE };
    set_attr(hwnd, DWMWA_SYSTEMBACKDROP_TYPE, &kind);

    // lets per-pixel alpha from our Direct2D drawing through to DWM
    let m = MARGINS {
        cxLeftWidth: -1,
        cxRightWidth: -1,
        cyTopHeight: -1,
        cyBottomHeight: -1,
    };
    unsafe {
        let _ = DwmExtendFrameIntoClientArea(hwnd, &m);
    }
}

/// Draws this app's context menus (and tooltips) in Windows 11's dark style, to
/// match the dark glass bar. Uses uxtheme's SetPreferredAppMode (ordinal 135,
/// undocumented but used by dark-mode apps) and FlushMenuThemes (ordinal 136).
pub fn use_dark_menus() {
    use windows::Win32::System::LibraryLoader::LoadLibraryW;
    use windows::core::PCSTR;
    const FORCE_DARK: i32 = 2;
    unsafe {
        let Ok(uxtheme) = LoadLibraryW(w!("uxtheme.dll")) else { return };
        if let Some(f) = GetProcAddress(uxtheme, PCSTR(135 as *const u8)) {
            let set_preferred_app_mode: unsafe extern "system" fn(i32) -> i32 = std::mem::transmute(f);
            set_preferred_app_mode(FORCE_DARK);
        }
        if let Some(f) = GetProcAddress(uxtheme, PCSTR(136 as *const u8)) {
            let flush_menu_themes: unsafe extern "system" fn() = std::mem::transmute(f);
            flush_menu_themes();
        }
    }
}

/// Color to clear the window with before drawing: transparent for acrylic (the
/// tint is part of the accent), the background color otherwise.
pub fn clear_color(cfg: &Config) -> D2D1_COLOR_F {
    if cfg.backdrop == Backdrop::Acrylic {
        D2D1_COLOR_F { r: 0.0, g: 0.0, b: 0.0, a: 0.0 }
    } else {
        parse_color(&cfg.background)
    }
}
