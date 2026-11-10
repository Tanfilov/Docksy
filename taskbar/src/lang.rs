//! Input language indicator ("ENG") and switching between the installed
//! keyboard layouts, like the one on Windows' taskbar.

use windows::Win32::{
    Foundation::{HWND, LPARAM, WPARAM},
    Globalization::{GetLocaleInfoEx, LCIDToLocaleName, LOCALE_SISO639LANGNAME2, LOCALE_SLOCALIZEDDISPLAYNAME},
    System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW},
    UI::{
        Input::KeyboardAndMouse::{GetKeyboardLayout, GetKeyboardLayoutList, HKL},
        WindowsAndMessaging::{GetWindowThreadProcessId, PostMessageW, WM_INPUTLANGCHANGEREQUEST},
    },
};
use windows::core::{HSTRING, PCWSTR};

#[derive(Clone, PartialEq, Debug, Default)]
pub struct InputLang {
    pub hkl: isize,
    /// "ENG"
    pub short: String,
    /// "English (United States)"
    pub name: String,
    /// keyboard layout, "US" / "Russian"
    pub layout: String,
}

impl InputLang {
    pub fn menu_label(&self) -> String {
        if self.layout.is_empty() || self.layout == self.name {
            format!("{}\t{}", self.name, self.short)
        } else {
            format!("{} — {} keyboard\t{}", self.name, self.layout, self.short)
        }
    }
}

fn locale_info(locale: &[u16], kind: u32) -> String {
    let mut buf = [0u16; 128];
    let n = unsafe { GetLocaleInfoEx(PCWSTR(locale.as_ptr()), kind, Some(&mut buf)) };
    if n <= 1 {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..n as usize - 1])
}

/// Name of the keyboard layout from the registry ("US", "Russian").
fn layout_text(hkl: isize) -> String {
    let lang = (hkl as usize & 0xFFFF) as u32;
    let device = ((hkl as usize >> 16) & 0xFFFF) as u32;
    // plain layouts: the device id is the layout's id; F-prefixed ids are variants,
    // fall back to the language's default layout for those
    let klid = if device & 0xF000 == 0xF000 { lang } else { device };
    let key = HSTRING::from(format!("SYSTEM\\CurrentControlSet\\Control\\Keyboard Layouts\\{klid:08X}"));
    let mut buf = [0u16; 128];
    let mut size = (buf.len() * 2) as u32;
    let ok = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            &key,
            windows::core::w!("Layout Text"),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut _),
            Some(&mut size),
        )
    }
    .is_ok();
    if !ok {
        return String::new();
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

pub fn describe(hkl: isize) -> InputLang {
    let langid = (hkl as usize & 0xFFFF) as u32;
    let mut name = [0u16; 85];
    let n = unsafe { LCIDToLocaleName(langid, Some(&mut name), 0) };
    let mut out = InputLang { hkl, layout: layout_text(hkl), ..Default::default() };
    if n > 1 {
        let locale = &name[..n as usize]; // includes the terminating 0
        out.short = locale_info(locale, LOCALE_SISO639LANGNAME2).to_uppercase();
        out.name = locale_info(locale, LOCALE_SLOCALIZEDDISPLAYNAME);
    }
    if out.short.is_empty() {
        out.short = format!("{langid:04X}");
    }
    out
}

/// The input language of a window (the one that gets typing).
pub fn of_window(hwnd: HWND) -> isize {
    unsafe {
        let thread = GetWindowThreadProcessId(hwnd, None);
        GetKeyboardLayout(thread).0 as isize
    }
}

/// Installed input languages, in Windows' order.
pub fn installed() -> Vec<InputLang> {
    unsafe {
        let n = GetKeyboardLayoutList(None);
        if n <= 0 {
            return Vec::new();
        }
        let mut list = vec![HKL::default(); n as usize];
        let n = GetKeyboardLayoutList(Some(&mut list));
        list.truncate(n.max(0) as usize);
        list.iter().map(|h| describe(h.0 as isize)).collect()
    }
}

/// Switches `target` (and with it the current input language) to `hkl`.
pub fn activate(target: HWND, hkl: isize) {
    unsafe {
        let _ = PostMessageW(Some(target), WM_INPUTLANGCHANGEREQUEST, WPARAM(0), LPARAM(hkl));
    }
}
