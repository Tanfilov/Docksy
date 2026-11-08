use std::{io::Write, path::PathBuf};

use windows::Win32::{
    Foundation::HWND,
    Graphics::Direct2D::Common::D2D1_COLOR_F,
    System::Threading::{
        OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        QueryFullProcessImageNameW,
    },
    UI::{
        Input::KeyboardAndMouse::{
            INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP,
            SendInput, VIRTUAL_KEY,
        },
        Shell::ShellExecuteW,
        WindowsAndMessaging::{GetClassNameW, GetWindowTextW, SW_SHOWNORMAL},
    },
};
use windows::core::{HSTRING, PCWSTR, PWSTR, w};

#[macro_export]
macro_rules! log {
    ($($t:tt)*) => { $crate::util::log_line(&format!($($t)*)) };
}

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn from_wide(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

/// `%APPDATA%\seelena` (config, pins).
pub fn data_dir() -> PathBuf {
    let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".into());
    let dir = PathBuf::from(base).join("seelena");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

pub fn log_line(msg: &str) {
    let path = data_dir().join("seelena.log");
    // keep the log small
    if std::fs::metadata(&path).map(|m| m.len() > 1_000_000).unwrap_or(false) {
        let _ = std::fs::remove_file(&path);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(f, "[{t}] {msg}");
    }
}

/// `#RRGGBB` or `#RRGGBBAA`.
pub fn parse_color(s: &str) -> D2D1_COLOR_F {
    let h = s.trim().trim_start_matches('#');
    let byte = |i: usize| -> f32 {
        h.get(i..i + 2)
            .and_then(|x| u8::from_str_radix(x, 16).ok())
            .unwrap_or(255) as f32
            / 255.0
    };
    D2D1_COLOR_F {
        r: byte(0),
        g: byte(2),
        b: byte(4),
        a: if h.len() >= 8 { byte(6) } else { 1.0 },
    }
}

pub fn window_class(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    let len = unsafe { GetClassNameW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..len.max(0) as usize])
}

pub fn window_title(hwnd: HWND) -> String {
    let mut buf = [0u16; 512];
    let len = unsafe { GetWindowTextW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..len.max(0) as usize])
}

pub fn process_path(pid: u32) -> Option<String> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut size = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut size);
        let _ = windows::Win32::Foundation::CloseHandle(h);
        ok.ok()?;
        Some(String::from_utf16_lossy(&buf[..size as usize]))
    }
}

/// AppUserModelID of a packaged (Store/MSIX) process.
pub fn process_aumid(pid: u32) -> Option<String> {
    use windows::Win32::Storage::Packaging::Appx::GetApplicationUserModelId;
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 512];
        let mut len = buf.len() as u32;
        let err = GetApplicationUserModelId(h, &mut len, Some(PWSTR(buf.as_mut_ptr())));
        let _ = windows::Win32::Foundation::CloseHandle(h);
        if err.0 != 0 {
            return None;
        }
        let s = from_wide(&buf);
        (!s.is_empty()).then_some(s)
    }
}

/// Press the keys in order, then release them in reverse order.
pub fn send_keys(keys: &[VIRTUAL_KEY]) {
    let key = |vk: VIRTUAL_KEY, flags: KEYBD_EVENT_FLAGS| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    let mut inputs: Vec<INPUT> = keys.iter().map(|&k| key(k, KEYBD_EVENT_FLAGS(0))).collect();
    inputs.extend(keys.iter().rev().map(|&k| key(k, KEYEVENTF_KEYUP)));
    unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
}

/// Opens a file, folder, URI or `shell:` path.
pub fn shell_open(target: &str, args: Option<&str>) {
    let (file, params) = if target.to_ascii_lowercase().starts_with("shell:") {
        ("explorer.exe".to_string(), Some(target.to_string()))
    } else {
        (target.to_string(), args.map(|a| a.to_string()))
    };
    let file = HSTRING::from(file);
    let params = params.map(HSTRING::from);
    unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            &file,
            params.as_ref().map(|p| PCWSTR(p.as_ptr())).unwrap_or(PCWSTR::null()),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}

pub fn file_stem(path: &str) -> String {
    std::path::Path::new(path)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string())
}
