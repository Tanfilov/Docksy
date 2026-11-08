//! Pinned apps, stored one per line in `%APPDATA%\seelena\pins.txt`.
//! A line is a `.lnk`/`.exe` path or `shell:AppsFolder\<AUMID>`.
//! On first run they are imported from the Windows taskbar (logic from Seelen UI's
//! `widgets/weg/native_taskbar.rs`).

use std::path::PathBuf;

use windows::Win32::{
    Storage::{EnhancedStorage::PKEY_AppUserModel_ID, FileSystem::WIN32_FIND_DATAW},
    System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, IPersistFile, STGM_READ},
    UI::Shell::{
        IShellItem, IShellLinkW, PropertiesSystem::IPropertyStore, SHCreateItemFromParsingName,
        SIGDN_NORMALDISPLAY, ShellLink,
    },
};
use windows::core::{HSTRING, Interface};
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_BINARY, RegGetValueW};

use crate::util::{self, data_dir};

const APPS_FOLDER: &str = "shell:appsfolder\\";

#[derive(Clone, Debug)]
pub struct Pin {
    /// the line in pins.txt
    pub path: String,
    pub name: String,
    /// target executable
    pub exe: Option<String>,
    pub aumid: Option<String>,
}

impl Pin {
    pub fn resolve(path: &str) -> Pin {
        let lower = path.to_lowercase();
        if lower.starts_with(APPS_FOLDER) {
            let aumid = path[APPS_FOLDER.len()..].to_string();
            return Pin {
                path: path.to_string(),
                name: shell_display_name(path).unwrap_or_else(|| aumid.clone()),
                exe: None,
                aumid: Some(aumid),
            };
        }
        if lower.ends_with(".lnk") {
            let (mut exe, aumid) = read_lnk(path).unwrap_or((None, None));
            // the File Explorer shortcut points to a shell folder, not to explorer.exe
            if exe.is_none() && aumid.as_deref() == Some("Microsoft.Windows.Explorer") {
                let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
                exe = Some(format!(r"{windir}\explorer.exe"));
            }
            return Pin {
                path: path.to_string(),
                name: util::file_stem(path),
                exe,
                aumid,
            };
        }
        Pin {
            path: path.to_string(),
            name: util::file_stem(path),
            exe: Some(path.to_string()),
            aumid: None,
        }
    }

    pub fn icon_sources(&self) -> Vec<String> {
        let mut v = vec![self.path.clone()];
        if let Some(e) = &self.exe {
            v.push(e.clone());
        }
        v
    }
}

fn shell_display_name(parsing: &str) -> Option<String> {
    unsafe {
        let item: IShellItem = SHCreateItemFromParsingName(&HSTRING::from(parsing), None).ok()?;
        let p = item.GetDisplayName(SIGDN_NORMALDISPLAY).ok()?;
        let s = p.to_string().ok();
        windows::Win32::System::Com::CoTaskMemFree(Some(p.0 as *const _));
        s
    }
}

fn read_lnk(path: &str) -> Option<(Option<String>, Option<String>)> {
    unsafe {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).ok()?;
        link.cast::<IPersistFile>()
            .ok()?
            .Load(&HSTRING::from(path), STGM_READ)
            .ok()?;
        let mut buf = [0u16; 1024];
        let mut fd = WIN32_FIND_DATAW::default();
        let exe = link
            .GetPath(&mut buf, &mut fd, 0)
            .ok()
            .map(|_| util::from_wide(&buf))
            .filter(|s| !s.is_empty());
        let aumid = link
            .cast::<IPropertyStore>()
            .ok()
            .and_then(|s| s.GetValue(&PKEY_AppUserModel_ID).ok())
            .map(|v| v.to_string())
            .filter(|s| !s.is_empty());
        Some((exe, aumid))
    }
}

fn pins_file() -> PathBuf {
    data_dir().join("pins.txt")
}

pub fn load() -> Vec<Pin> {
    let file = pins_file();
    if !file.exists() {
        let imported = import_from_windows();
        crate::log!("imported {} pins from the Windows taskbar", imported.len());
        save(&imported);
    }
    std::fs::read_to_string(&file)
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(Pin::resolve)
        .collect()
}

pub fn save(paths: &[String]) {
    let mut text = String::from(
        "# Pinned apps, one per line: a .lnk/.exe path or shell:AppsFolder\\<AppUserModelID>\n",
    );
    for p in paths {
        text.push_str(p);
        text.push('\n');
    }
    let _ = std::fs::write(pins_file(), text);
}

fn import_from_windows() -> Vec<String> {
    let folder = PathBuf::from(std::env::var("APPDATA").unwrap_or_default())
        .join(r"Microsoft\Internet Explorer\Quick Launch\User Pinned\TaskBar");
    let mut result: Vec<String> = registry_pin_order()
        .into_iter()
        .filter(|p| std::path::Path::new(p).exists())
        .collect();
    // anything in the folder that the registry blob didn't list, oldest first
    if let Ok(entries) = std::fs::read_dir(&folder) {
        let mut extra: Vec<(std::time::SystemTime, String)> = entries
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x.eq_ignore_ascii_case("lnk")))
            .map(|e| {
                let t = e.metadata().and_then(|m| m.created()).unwrap_or(std::time::UNIX_EPOCH);
                (t, e.path().to_string_lossy().to_string())
            })
            .filter(|(_, p)| !result.iter().any(|r| r.eq_ignore_ascii_case(p)))
            .collect();
        extra.sort();
        result.extend(extra.into_iter().map(|(_, p)| p));
    }
    let mut seen = std::collections::HashSet::new();
    result.retain(|p| seen.insert(p.to_lowercase()));
    result
}

/// The pin order lives in `HKCU\...\Explorer\Taskband\FavoritesResolve`, a binary blob
/// that embeds the `.lnk` paths as single-byte ASCII runs.
fn registry_pin_order() -> Vec<String> {
    let mut size = 0u32;
    let key = HSTRING::from(r"Software\Microsoft\Windows\CurrentVersion\Explorer\Taskband");
    let val = HSTRING::from("FavoritesResolve");
    unsafe {
        if RegGetValueW(HKEY_CURRENT_USER, &key, &val, RRF_RT_REG_BINARY, None, None, Some(&mut size))
            .is_err()
            || size == 0
        {
            return Vec::new();
        }
        let mut buf = vec![0u8; size as usize];
        if RegGetValueW(
            HKEY_CURRENT_USER,
            &key,
            &val,
            RRF_RT_REG_BINARY,
            None,
            Some(buf.as_mut_ptr() as *mut _),
            Some(&mut size),
        )
        .is_err()
        {
            return Vec::new();
        }
        let text: String = buf.iter().map(|&b| b as char).collect();
        let profile = std::env::var("USERPROFILE").unwrap_or_default();
        if profile.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut rest = text.as_str();
        while let Some(start) = rest.find(&profile) {
            let tail = &rest[start..];
            // path ends at the first ".lnk"
            match tail.to_ascii_lowercase().find(".lnk") {
                Some(end) => {
                    let p = &tail[..end + 4];
                    if !p.contains('\0') {
                        out.push(p.to_string());
                    }
                    rest = &tail[end + 4..];
                }
                None => break,
            }
        }
        out
    }
}
