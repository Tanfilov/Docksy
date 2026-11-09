//! Docksy's own Start panel: search box, pinned apps, all apps, and a footer with
//! the account (lock, sign out) and power (sleep, shut down, restart) buttons.
//!
//! Windows' Start menu can't be placed next to our button (it always opens from
//! its own hidden taskbar), so the Start button (and the Win key, unless
//! `win_key_opens_start = false`) opens this panel instead. Apps come from Windows' "Apps" folder
//! (`shell:AppsFolder`), the same list Start shows, desktop and Store apps alike.
//! Pins are Docksy's own, kept in `%APPDATA%\Docksy\start_pins.txt`.
//!
//! Search also finds what Windows' Start search finds beyond that list: the Windows
//! tools (Command Prompt, PowerShell, Registry Editor, Services, ...), by name or
//! command ("cmd", "regedit"), programs on the PATH, and runs the typed text as a
//! command like Win+R. Tab completes the top match.

use std::{
    collections::{HashMap, HashSet},
    sync::Mutex,
    time::{Duration, Instant},
};

use windows::Win32::{
    Foundation::{HWND, RECT},
    Graphics::{
        Direct2D::{
            Common::{D2D_RECT_F, D2D_SIZE_U, D2D1_COLOR_F},
            D2D1_ANTIALIAS_MODE_PER_PRIMITIVE, D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
            D2D1_DRAW_TEXT_OPTIONS_CLIP, D2D1_ROUNDED_RECT, ID2D1Bitmap, ID2D1Factory,
            ID2D1HwndRenderTarget, ID2D1SolidColorBrush,
        },
        DirectWrite::{
            DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT,
            DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_MEASURING_MODE_NATURAL,
            DWRITE_PARAGRAPH_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT, DWRITE_TEXT_ALIGNMENT_CENTER,
            DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_TEXT_ALIGNMENT_TRAILING, DWRITE_TRIMMING, DWRITE_TRIMMING_GRANULARITY_CHARACTER,
            DWRITE_WORD_WRAPPING_NO_WRAP, IDWriteFactory, IDWriteTextFormat,
        },
        Gdi::InvalidateRect,
    },
    System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree},
    UI::{
        Shell::{
            BHID_EnumItems, IEnumShellItems, IShellItem, SHCreateItemFromParsingName,
            SIGDN_NORMALDISPLAY, SIGDN_PARENTRELATIVEPARSING,
        },
        WindowsAndMessaging::{
            CreateWindowExW, HWND_TOPMOST, SW_HIDE, SWP_SHOWWINDOW,
            SetWindowPos, ShowWindow, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
        },
    },
};
use windows::core::w;

use crate::{
    bar::{cached_bitmap, ensure_target, rf},
    config::{Config, Position},
    icons::{IconCache, IconLoader, Image},
    util::{self, parse_color},
};

#[derive(Clone, Debug)]
pub struct AppEntry {
    pub name: String,
    /// id inside the Apps folder (an AppUserModelID or a known-folder path)
    pub id: String,
    /// shown in "All apps"; false = found by search only (Windows tools)
    pub listed: bool,
    /// command that finds it too ("cmd" for Command Prompt), lowercase
    pub key: String,
}

/// A program or Windows tool that isn't in the Apps folder, found by search.
#[derive(Clone, Debug)]
pub struct Command {
    pub name: String,
    /// full path
    pub path: String,
    pub args: String,
    /// lowercase command ("regedit"), empty when it has none
    pub key: String,
    /// a program from the PATH (shown only for close matches)
    pub extra: bool,
}

/// Windows tools Start search finds: (name, file, arguments).
const TOOLS: &[(&str, &str, &str)] = &[
    ("Command Prompt", "cmd.exe", ""),
    ("Windows PowerShell", "powershell.exe", ""),
    ("PowerShell 7", "pwsh.exe", ""),
    ("Windows PowerShell ISE", "powershell_ise.exe", ""),
    ("Terminal", "wt.exe", ""),
    ("Registry Editor", "regedit.exe", ""),
    ("Control Panel", "control.exe", ""),
    ("Windows Tools", "control.exe", "admintools"),
    ("Task Manager", "taskmgr.exe", ""),
    ("Services", "services.msc", ""),
    ("System Configuration", "msconfig.exe", ""),
    ("Device Manager", "devmgmt.msc", ""),
    ("Disk Management", "diskmgmt.msc", ""),
    ("Computer Management", "compmgmt.msc", ""),
    ("Event Viewer", "eventvwr.msc", ""),
    ("Task Scheduler", "taskschd.msc", ""),
    ("Local Group Policy Editor", "gpedit.msc", ""),
    ("Local Security Policy", "secpol.msc", ""),
    ("Local Users and Groups", "lusrmgr.msc", ""),
    ("Windows Defender Firewall with Advanced Security", "wf.msc", ""),
    ("Certificates", "certmgr.msc", ""),
    ("Hyper-V Manager", "virtmgmt.msc", ""),
    ("Resource Monitor", "resmon.exe", ""),
    ("Performance Monitor", "perfmon.exe", ""),
    ("System Information", "msinfo32.exe", ""),
    ("DirectX Diagnostic Tool", "dxdiag.exe", ""),
    ("Remote Desktop Connection", "mstsc.exe", ""),
    ("Character Map", "charmap.exe", ""),
    ("On-Screen Keyboard", "osk.exe", ""),
    ("Magnifier", "magnify.exe", ""),
    ("Notepad", "notepad.exe", ""),
    ("Paint", "mspaint.exe", ""),
    ("WordPad", "write.exe", ""),
    ("Windows Features", "optionalfeatures.exe", ""),
    ("Programs and Features", "appwiz.cpl", ""),
    ("Network Connections", "ncpa.cpl", ""),
    ("Sound", "mmsys.cpl", ""),
    ("Power Options", "powercfg.cpl", ""),
    ("Mouse", "main.cpl", ""),
    ("Date and Time", "timedate.cpl", ""),
    ("System Properties", "sysdm.cpl", ""),
    ("Edit environment variables", "rundll32.exe", "sysdm.cpl,EditEnvironmentVariables"),
    ("User Accounts", "netplwiz.exe", ""),
    ("Disk Cleanup", "cleanmgr.exe", ""),
    ("Defragment and Optimize Drives", "dfrgui.exe", ""),
    ("Windows Memory Diagnostic", "mdsched.exe", ""),
    ("ODBC Data Sources", "odbcad32.exe", ""),
    ("Recovery Drive", "recoverydrive.exe", ""),
    ("Steps Recorder", "psr.exe", ""),
    ("Run", "explorer.exe", "shell:::{2559a1f3-21d7-11d4-bdaf-00c04f60b9f0}"),
];

fn search_dirs() -> Vec<std::path::PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(root) = std::env::var("SystemRoot") {
        dirs.push(std::path::Path::new(&root).join("System32"));
        dirs.push(std::path::PathBuf::from(root));
    }
    if let Ok(path) = std::env::var("PATH") {
        dirs.extend(std::env::split_paths(&path));
    }
    dirs
}

/// Full path of a file in the system folders or on the PATH. App execution
/// aliases (wt.exe, python.exe) are empty reparse points: `exists()` can't see them.
fn resolve(file: &str, dirs: &[std::path::PathBuf]) -> Option<String> {
    dirs.iter()
        .map(|d| d.join(file))
        .find(|p| std::fs::symlink_metadata(p).is_ok())
        .map(|p| p.to_string_lossy().to_string())
}

/// Windows tools and PATH programs. Tools that are already in `apps` (e.g. the
/// "Command Prompt" shortcut) give their command to that entry instead.
pub fn enumerate_commands(apps: &mut [AppEntry]) -> Vec<Command> {
    let dirs = search_dirs();
    let mut cmds = Vec::new();
    let mut known: HashSet<String> = HashSet::new();
    for &(name, file, args) in TOOLS {
        let stem = file.rsplit_once('.').map(|(s, _)| s).unwrap_or(file).to_lowercase();
        let key = if args.is_empty() { stem.clone() } else { String::new() };
        if let Some(app) = apps.iter_mut().find(|a| name_key(&a.name) == name_key(name)) {
            if app.key.is_empty() {
                app.key = key.clone();
            }
            known.insert(stem);
            continue;
        }
        let Some(path) = resolve(file, &dirs) else { continue };
        known.insert(stem);
        cmds.push(Command { name: name.to_string(), path, args: args.to_string(), key, extra: false });
    }
    let mut extras: Vec<Command> = Vec::new();
    for dir in &dirs {
        let Ok(entries) = std::fs::read_dir(dir) else { continue };
        for e in entries.flatten() {
            let file = e.file_name().to_string_lossy().to_string();
            let Some(stem) = file.strip_suffix(".exe").or_else(|| file.strip_suffix(".EXE")) else { continue };
            let key = stem.to_lowercase();
            if !known.insert(key.clone()) {
                continue;
            }
            let path = e.path().to_string_lossy().to_string();
            extras.push(Command { name: stem.to_string(), path, args: String::new(), key, extra: true });
        }
    }
    extras.sort_by(|a, b| a.key.cmp(&b.key));
    cmds.extend(extras);
    cmds
}

/// Splits typed text into program and arguments, like the Run box.
fn parse_command(text: &str) -> Option<(String, Option<String>)> {
    let t = text.trim();
    if t.is_empty() {
        return None;
    }
    if let Some(rest) = t.strip_prefix('"') {
        let (file, args) = rest.split_once('"').unwrap_or((rest, ""));
        let args = args.trim();
        return Some((file.to_string(), (!args.is_empty()).then(|| args.to_string())));
    }
    // a path with spaces, typed without quotes
    if std::path::Path::new(t).exists() {
        return Some((t.to_string(), None));
    }
    match t.split_once(char::is_whitespace) {
        Some((file, args)) => Some((file.to_string(), Some(args.trim().to_string()))),
        None => Some((t.to_string(), None)),
    }
}

/// How well a name / command matches the search text (lowercase); lower is better.
fn score(name: &str, key: &str, q: &str) -> Option<u8> {
    let n = name.to_lowercase();
    if n.starts_with(q) || (!key.is_empty() && key == q) {
        return Some(0);
    }
    if (!key.is_empty() && key.starts_with(q)) || n.split(|c: char| !c.is_alphanumeric()).any(|w| w.starts_with(q)) {
        return Some(1);
    }
    n.contains(q).then_some(2)
}

pub fn shell_path(id: &str) -> String {
    format!("shell:AppsFolder\\{id}")
}

// ---------------------------------------------------------------------------
// app list (built on a worker thread) and pins
// ---------------------------------------------------------------------------

/// Results of the last background enumeration, picked up by the UI thread.
pub static ENUMERATED: Mutex<Option<(Vec<AppEntry>, Vec<Command>)>> = Mutex::new(None);

/// Lists everything in Windows' Apps folder. Runs on a worker thread.
pub fn enumerate_apps() -> Vec<AppEntry> {
    let mut apps = Vec::new();
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let Ok(folder) = SHCreateItemFromParsingName::<_, _, IShellItem>(w!("shell:AppsFolder"), None) else {
            return apps;
        };
        let Ok(items) = folder.BindToHandler::<_, IEnumShellItems>(None, &BHID_EnumItems) else {
            return apps;
        };
        loop {
            let mut batch: [Option<IShellItem>; 32] = Default::default();
            let mut fetched = 0u32;
            if items.Next(&mut batch, Some(&mut fetched)).is_err() || fetched == 0 {
                break;
            }
            for item in batch.iter().take(fetched as usize).flatten() {
                let text = |kind| -> Option<String> {
                    let p = item.GetDisplayName(kind).ok()?;
                    let s = p.to_string().ok();
                    CoTaskMemFree(Some(p.0 as *const _));
                    s
                };
                if let (Some(name), Some(id)) = (text(SIGDN_NORMALDISPLAY), text(SIGDN_PARENTRELATIVEPARSING)) {
                    if !name.is_empty() && !id.is_empty() {
                        apps.push(AppEntry { name, id, listed: true, key: String::new() });
                    }
                }
            }
        }
    }
    let hidden = hidden_shortcut_names();
    apps.retain_mut(|a| match is_start_app(a, &hidden) {
        Shown::Listed => true,
        Shown::SearchOnly => {
            a.listed = false;
            true
        }
        Shown::Never => false,
    });
    apps.sort_by_key(|a| (a.name.to_lowercase(), !a.listed));
    // same app listed twice (e.g. a Store and a desktop entry): keep one
    apps.dedup_by(|a, b| a.id == b.id || a.name.eq_ignore_ascii_case(&b.name));
    apps
}

/// Start Menu folders whose shortcuts Windows 11 Start doesn't list (it shows a
/// single "Windows Tools" entry for them instead).
const TOOL_FOLDERS: &[&str] = &[
    "administrative tools",
    "system tools",
    "accessibility",
    "accessories",
    "windows powershell",
    "windows tools",
    "microsoft office tools",
];

/// Comparison key for app names: letters and digits only ("Disk Clean-up" = "Disk Cleanup").
fn name_key(name: &str) -> String {
    name.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

/// The name Windows shows for a shortcut (respects localized names from
/// desktop.ini, e.g. `dfrgui.lnk` is shown as "Defragment and Optimize Drives").
fn shell_display_name(path: &std::path::Path) -> Option<String> {
    unsafe {
        let item: IShellItem =
            SHCreateItemFromParsingName(&windows::core::HSTRING::from(path.as_os_str()), None).ok()?;
        let p = item.GetDisplayName(SIGDN_NORMALDISPLAY).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const _));
        s
    }
}

/// Names of shortcuts that Windows' Start "All" list doesn't show: those inside the
/// Windows tool folders, and those buried two or more folders deep (e.g. the
/// "Visual Studio 2022\Visual Studio Tools\..." prompts or codec settings).
fn hidden_shortcut_names() -> HashSet<String> {
    let mut names = HashSet::new();
    let roots = [
        std::env::var("APPDATA").map(|p| format!(r"{p}\Microsoft\Windows\Start Menu\Programs")),
        std::env::var("ProgramData").map(|p| format!(r"{p}\Microsoft\Windows\Start Menu\Programs")),
    ];
    fn walk(dir: &std::path::Path, depth: usize, tool_folder: bool, names: &mut HashSet<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for e in entries.flatten() {
            let path = e.path();
            if path.is_dir() {
                let folder = path.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
                let tools = tool_folder || (depth == 0 && TOOL_FOLDERS.contains(&folder.as_str()));
                walk(&path, depth + 1, tools, names);
            } else if (tool_folder || depth >= 2)
                && path.extension().is_some_and(|x| x.eq_ignore_ascii_case("lnk"))
            {
                if let Some(stem) = path.file_stem() {
                    names.insert(name_key(&stem.to_string_lossy()));
                }
                if let Some(shown) = shell_display_name(&path) {
                    names.insert(name_key(&shown));
                }
            }
        }
    }
    for root in roots.into_iter().flatten() {
        walk(std::path::Path::new(&root), 0, false, &mut names);
    }
    names
}

enum Shown {
    Listed,
    /// Windows tools: not in "All apps", but search finds them
    SearchOnly,
    Never,
}

/// Whether an Apps-folder entry is a regular app, as Windows' Start "All" list
/// shows: no web links, documents, uninstallers, help pages, or Windows tools.
fn is_start_app(app: &AppEntry, hidden: &HashSet<String>) -> Shown {
    let id = app.id.to_lowercase();
    let name = app.name.to_lowercase();
    // Store/packaged apps (Family!App) are always real apps
    if id.contains('!') {
        return Shown::Listed;
    }
    if id.starts_with("http:") || id.starts_with("https:") {
        return Shown::Never;
    }
    // file targets: only programs (and the tools' consoles, .msc)
    let mut tool_file = false;
    if id.contains('\\') {
        let ext = id.rsplit('.').next().unwrap_or("");
        if ext == "msc" {
            tool_file = true;
        } else if ext != "exe" {
            return Shown::Never;
        }
        let file = id.rsplit('\\').next().unwrap_or("");
        if file.starts_with("unins") || file.starts_with("uninst") || file.starts_with("uninstall") {
            return Shown::Never;
        }
    }
    if name.starts_with("uninstall") {
        return Shown::Never;
    }
    if hidden.contains(&name_key(&name)) || tool_file {
        return Shown::SearchOnly;
    }
    const NOT_APPS: &[&str] = &[
        "documentation",
        "release notes",
        "readme",
        "read me",
        "manual",
        " help",
        "faq",
        "website",
        "on the web",
        "support center",
        "module docs",
    ];
    if NOT_APPS.iter().any(|w| name.contains(w)) { Shown::Never } else { Shown::Listed }
}

fn pins_file() -> std::path::PathBuf {
    util::data_dir().join("start_pins.txt")
}

fn load_pins() -> Vec<String> {
    std::fs::read_to_string(pins_file())
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect()
}

fn save_pins(pins: &[String]) {
    let mut text = String::from("# Docksy Start pins: one Apps-folder id per line (right-click an app to pin)\n");
    for p in pins {
        text.push_str(p);
        text.push('\n');
    }
    let _ = std::fs::write(pins_file(), text);
}

// ---------------------------------------------------------------------------
// panel
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StartHit {
    /// index into `pins`
    Pinned(usize),
    /// index into `apps`
    App(usize),
    /// index into `commands`
    Cmd(usize),
    /// run the search text as a command, like Win+R
    Run,
    /// footer: account button (lock, sign out)
    User,
    /// footer: power button (sleep, shut down, restart)
    Power,
}

pub struct StartMenu {
    pub hwnd: HWND,
    rt: Option<ID2D1HwndRenderTarget>,
    bitmaps: HashMap<u64, ID2D1Bitmap>,
    pub open: bool,
    pub closed_at: Instant,
    pub apps: Vec<AppEntry>,
    pub commands: Vec<Command>,
    apps_loaded_at: Option<Instant>,
    pub pins: Vec<String>,
    pub query: String,
    scroll: f32,
    max_scroll: f32,
    row_h: f32,
    items: Vec<(StartHit, D2D_RECT_F)>,
    pub hover: Option<StartHit>,
    pub pressed: Option<StartHit>,
    pub tracking: bool,
    scale: f32,
    size: (i32, i32),
    user_name: String,
}

impl StartMenu {
    pub fn new(hinst: windows::Win32::Foundation::HINSTANCE) -> Option<StartMenu> {
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
                w!("DocksyStart"),
                w!("Docksy Start"),
                WS_POPUP,
                0,
                0,
                1,
                1,
                None,
                None,
                Some(hinst),
                None,
            )
        }
        .ok()?;
        Some(StartMenu {
            hwnd,
            rt: None,
            bitmaps: HashMap::new(),
            open: false,
            closed_at: Instant::now(),
            apps: Vec::new(),
            commands: Vec::new(),
            apps_loaded_at: None,
            pins: load_pins(),
            query: String::new(),
            scroll: 0.0,
            max_scroll: 0.0,
            row_h: 40.0,
            items: Vec::new(),
            hover: None,
            pressed: None,
            tracking: false,
            scale: 1.0,
            size: (1, 1),
            user_name: crate::util::user_display_name(),
        })
    }

    /// True when the app list should be (re)built: never loaded, or stale.
    pub fn wants_refresh(&self) -> bool {
        self.apps_loaded_at.is_none_or(|t| t.elapsed() > Duration::from_secs(60))
    }

    pub fn set_apps(&mut self, apps: Vec<AppEntry>, commands: Vec<Command>) {
        self.apps = apps;
        self.commands = commands;
        self.apps_loaded_at = Some(Instant::now());
        self.invalidate();
    }

    pub fn invalidate(&self) {
        unsafe {
            let _ = InvalidateRect(Some(self.hwnd), None, false);
        }
    }

    pub fn name_of(&self, id: &str) -> String {
        self.apps
            .iter()
            .find(|a| a.id == id)
            .map(|a| a.name.clone())
            .unwrap_or_else(|| id.rsplit(['\\', '!']).next().unwrap_or(id).to_string())
    }

    pub fn is_pinned(&self, id: &str) -> bool {
        self.pins.iter().any(|p| p == id)
    }

    pub fn toggle_pin(&mut self, id: &str) {
        if self.is_pinned(id) {
            self.pins.retain(|p| p != id);
        } else {
            self.pins.push(id.to_string());
        }
        save_pins(&self.pins);
        self.invalidate();
    }

    /// Id for a hit (pinned tile or app row).
    pub fn id_of(&self, hit: StartHit) -> Option<String> {
        match hit {
            StartHit::Pinned(i) => self.pins.get(i).cloned(),
            StartHit::App(i) => self.apps.get(i).map(|a| a.id.clone()),
            StartHit::Cmd(_) | StartHit::Run | StartHit::User | StartHit::Power => None,
        }
    }

    /// What to open for a hit: (file, arguments).
    pub fn launch_target(&self, hit: StartHit) -> Option<(String, Option<String>)> {
        match hit {
            StartHit::Pinned(_) | StartHit::App(_) => self.id_of(hit).map(|id| (shell_path(&id), None)),
            StartHit::Cmd(i) => {
                let c = self.commands.get(i)?;
                Some((c.path.clone(), (!c.args.is_empty()).then(|| c.args.clone())))
            }
            StartHit::Run => parse_command(&self.query),
            StartHit::User | StartHit::Power => None,
        }
    }

    /// Whether "Run as administrator" makes sense (not for Store apps).
    pub fn can_elevate(&self, hit: StartHit) -> bool {
        match hit {
            StartHit::Cmd(_) | StartHit::Run => true,
            StartHit::Pinned(_) | StartHit::App(_) => self.id_of(hit).is_some_and(|id| !id.contains('!')),
            StartHit::User | StartHit::Power => false,
        }
    }

    /// Search results, best first (all listed apps when not searching). While
    /// searching, the last entry runs the typed text.
    pub fn filtered(&self) -> Vec<StartHit> {
        let q = self.query.trim().to_lowercase();
        if q.is_empty() {
            return (0..self.apps.len()).filter(|&i| self.apps[i].listed).map(StartHit::App).collect();
        }
        let mut ranked: Vec<(u8, StartHit)> = Vec::new();
        for (i, a) in self.apps.iter().enumerate() {
            if let Some(sc) = score(&a.name, &a.key, &q) {
                ranked.push((sc, StartHit::App(i)));
            }
        }
        let mut extras = 0;
        for (i, c) in self.commands.iter().enumerate() {
            if !c.extra {
                if let Some(sc) = score(&c.name, &c.key, &q) {
                    ranked.push((sc, StartHit::Cmd(i)));
                }
            } else if q.chars().count() >= 2 && extras < 5 && c.key.starts_with(&q) {
                // programs on the PATH: only close matches, after the named tools
                ranked.push((if c.key == q { 1 } else { 3 }, StartHit::Cmd(i)));
                extras += 1;
            }
        }
        ranked.sort_by_key(|(sc, _)| *sc);
        let mut out: Vec<StartHit> = ranked.into_iter().map(|(_, h)| h).collect();
        out.push(StartHit::Run);
        out
    }

    /// The top result's name (or command) when it continues the typed text:
    /// "reg" -> "Registry Editor". Shown dimmed after the text; Tab accepts it.
    pub fn completion(&self) -> Option<String> {
        if self.query.is_empty() {
            return None;
        }
        let (name, key) = match *self.filtered().first()? {
            StartHit::App(i) => (self.apps[i].name.as_str(), self.apps[i].key.as_str()),
            StartHit::Cmd(i) => (self.commands[i].name.as_str(), self.commands[i].key.as_str()),
            _ => return None,
        };
        let n = self.query.chars().count();
        let typed: String = self.query.to_lowercase();
        [name, key].into_iter().find_map(|cand| {
            if cand.chars().count() <= n {
                return None;
            }
            let head: String = cand.chars().take(n).collect::<String>().to_lowercase();
            (head == typed).then(|| format!("{}{}", self.query, cand.chars().skip(n).collect::<String>()))
        })
    }

    /// Tab: accept the completion.
    pub fn complete(&mut self) {
        if let Some(c) = self.completion() {
            self.query = c;
            self.scroll = 0.0;
            self.invalidate();
        }
    }

    pub fn hit_test(&self, x: i32, y: i32) -> Option<StartHit> {
        let (x, y) = (x as f32, y as f32);
        self.items
            .iter()
            .find(|(_, r)| x >= r.left && x < r.right && y >= r.top && y < r.bottom)
            .map(|(h, _)| *h)
    }

    pub fn scroll_by(&mut self, wheel_delta: i32) {
        let rows = wheel_delta as f32 / 120.0 * 3.0;
        self.scroll = (self.scroll - rows * self.row_h).clamp(0.0, self.max_scroll);
        self.invalidate();
    }

    pub fn type_char(&mut self, c: char) {
        self.query.push(c);
        self.scroll = 0.0;
        self.invalidate();
    }

    pub fn backspace(&mut self) {
        self.query.pop();
        self.scroll = 0.0;
        self.invalidate();
    }

    /// Opens next to the Start button. `button`, `bar` and `monitor` are screen rects.
    pub fn show(&mut self, cfg: &Config, button: RECT, bar: RECT, monitor: RECT, scale: f32) {
        self.scale = scale;
        self.query.clear();
        self.scroll = 0.0;
        self.hover = None;
        self.pins = load_pins();
        let s = scale;
        let gap = (8.0 * s).round() as i32;
        let w = ((600.0 * s).round() as i32).min(monitor.right - monitor.left - 2 * gap);
        let h = ((700.0 * s).round() as i32).min(monitor.bottom - monitor.top - 2 * gap);
        let (mut x, mut y) = match cfg.position {
            Position::Bottom => (button.left, bar.top - gap - h),
            Position::Top => (button.left, bar.bottom + gap),
            Position::Left => (bar.right + gap, button.top),
            Position::Right => (bar.left - gap - w, button.top),
        };
        x = x.clamp(monitor.left + gap, (monitor.right - gap - w).max(monitor.left));
        y = y.clamp(monitor.top + gap, (monitor.bottom - gap - h).max(monitor.top));
        self.size = (w, h);
        crate::backdrop::apply(self.hwnd, cfg, true);
        self.open = true;
        unsafe {
            let _ = SetWindowPos(self.hwnd, Some(HWND_TOPMOST), x, y, w, h, SWP_SHOWWINDOW);
            // focus, so typing goes to the search box
            crate::util::force_foreground(self.hwnd);
        }
        self.invalidate();
    }

    pub fn hide(&mut self) {
        if !self.open {
            return;
        }
        self.open = false;
        self.closed_at = Instant::now();
        self.hover = None;
        self.pressed = None;
        self.tracking = false;
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
    }

    fn format(
        dw: &IDWriteFactory,
        size: f32,
        weight: DWRITE_FONT_WEIGHT,
        align: DWRITE_TEXT_ALIGNMENT,
    ) -> Option<IDWriteTextFormat> {
        Self::format_font(dw, w!("Segoe UI Variable Text"), size, weight, align)
    }

    fn format_font(
        dw: &IDWriteFactory,
        font: windows::core::PCWSTR,
        size: f32,
        weight: DWRITE_FONT_WEIGHT,
        align: DWRITE_TEXT_ALIGNMENT,
    ) -> Option<IDWriteTextFormat> {
        unsafe {
            let f = dw
                .CreateTextFormat(
                    font,
                    None,
                    weight,
                    DWRITE_FONT_STYLE_NORMAL,
                    DWRITE_FONT_STRETCH_NORMAL,
                    size,
                    w!(""),
                )
                .ok()?;
            let _ = f.SetTextAlignment(align);
            let _ = f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER);
            let _ = f.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP);
            // long names end in "..."
            if let Ok(sign) = dw.CreateEllipsisTrimmingSign(&f) {
                let trimming = DWRITE_TRIMMING {
                    granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER,
                    delimiter: 0,
                    delimiterCount: 0,
                };
                let _ = f.SetTrimming(&trimming, &sign);
            }
            Some(f)
        }
    }

    fn text_width(dw: &IDWriteFactory, fmt: &IDWriteTextFormat, t: &str) -> f32 {
        let t: Vec<u16> = t.encode_utf16().collect();
        let Ok(layout) = (unsafe { dw.CreateTextLayout(&t, fmt, 10000.0, 1000.0) }) else { return 0.0 };
        let mut m = Default::default();
        match unsafe { layout.GetMetrics(&mut m) } {
            Ok(()) => {
                let m: windows::Win32::Graphics::DirectWrite::DWRITE_TEXT_METRICS = m;
                m.widthIncludingTrailingWhitespace
            }
            Err(_) => 0.0,
        }
    }

    /// Pixel size of pinned-tile and list icons (loaded 1.25x larger than drawn, see Image::fit).
    fn tile_px(s: f32) -> u32 {
        ((40.0 * s).round() * 1.25) as u32
    }

    fn row_px(s: f32) -> u32 {
        ((28.0 * s).round() * 1.25) as u32
    }

    pub fn set_scale(&mut self, scale: f32) {
        self.scale = scale;
    }

    /// Queues the icons the panel shows without searching (pins first, then the app
    /// list from the top, then the tools search finds), so scrolling never waits
    /// for one.
    pub fn preload_icons(&self, icons: &IconCache, loader: &mut IconLoader) {
        let s = self.scale;
        let mut want = |src: String, px: u32| {
            if icons.cached(&src, px).is_none() {
                loader.request(&src, px);
            }
        };
        for id in &self.pins {
            want(shell_path(id), Self::tile_px(s));
        }
        for a in self.apps.iter().filter(|a| a.listed) {
            want(shell_path(&a.id), Self::row_px(s));
        }
        for a in self.apps.iter().filter(|a| !a.listed) {
            want(shell_path(&a.id), Self::row_px(s));
        }
        for c in self.commands.iter().filter(|c| !c.extra) {
            want(c.path.clone(), Self::row_px(s));
        }
    }

    pub fn paint(
        &mut self,
        d2d: &ID2D1Factory,
        dw: &IDWriteFactory,
        cfg: &Config,
        icons: &mut IconCache,
        loader: &mut IconLoader,
    ) {
        // icons come from the cache only; missing ones are loaded in the background
        // and the panel repaints when they arrive
        let mut icon_for = |src: &str, px: u32| -> Option<std::rc::Rc<Image>> {
            match icons.cached(src, px) {
                Some(img) => img,
                None => {
                    loader.request(src, px);
                    None
                }
            }
        };
        if !self.open {
            return;
        }
        let size = D2D_SIZE_U {
            width: self.size.0.max(1) as u32,
            height: self.size.1.max(1) as u32,
        };
        let Some(rt) = ensure_target(&mut self.rt, d2d, self.hwnd, size) else { return };
        let s = self.scale;
        let (w, h) = (self.size.0 as f32, self.size.1 as f32);
        let pad = 16.0 * s;

        let fs = cfg.font_size * s;
        let (Some(name_fmt), Some(tile_fmt), Some(head_fmt), Some(search_fmt)) = (
            Self::format(dw, fs, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_TEXT_ALIGNMENT_LEADING),
            Self::format(dw, fs * 0.86, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_TEXT_ALIGNMENT_CENTER),
            Self::format(dw, fs, DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_TEXT_ALIGNMENT_LEADING),
            Self::format(dw, fs, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_TEXT_ALIGNMENT_LEADING),
        ) else {
            return;
        };
        let Some(key_fmt) = Self::format(dw, fs * 0.9, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_TEXT_ALIGNMENT_TRAILING) else {
            return;
        };
        let Some(glyph_fmt) = Self::format_font(
            dw,
            w!("Segoe Fluent Icons"),
            fs * 1.15,
            DWRITE_FONT_WEIGHT_NORMAL,
            DWRITE_TEXT_ALIGNMENT_CENTER,
        ) else {
            return;
        };
        let text = parse_color(&cfg.text);
        let dim = D2D1_COLOR_F { a: text.a * 0.6, ..text };
        let hover = parse_color(&cfg.hover);
        let field = D2D1_COLOR_F { r: 1.0, g: 1.0, b: 1.0, a: 0.08 };
        let accent = parse_color(&cfg.accent);
        let brush = |c: D2D1_COLOR_F| -> Option<ID2D1SolidColorBrush> { unsafe { rt.CreateSolidColorBrush(&c, None) }.ok() };
        let (Some(b_text), Some(b_dim), Some(b_hover), Some(b_field), Some(b_accent)) =
            (brush(text), brush(dim), brush(hover), brush(field), brush(accent))
        else {
            return;
        };
        let draw_text = |t: &str, fmt: &IDWriteTextFormat, r: D2D_RECT_F, b: &ID2D1SolidColorBrush| {
            let t: Vec<u16> = t.encode_utf16().collect();
            unsafe { rt.DrawText(&t, fmt, &r, b, D2D1_DRAW_TEXT_OPTIONS_CLIP, DWRITE_MEASURING_MODE_NATURAL) };
        };
        let radius = 6.0 * s;
        let mut used = HashSet::new();
        let mut items: Vec<(StartHit, D2D_RECT_F)> = Vec::new();
        let hover_hit = self.hover;

        unsafe {
            rt.BeginDraw();
            rt.Clear(Some(&crate::backdrop::clear_color(cfg)));

            // search box
            let mut y = pad;
            let sb = rf(pad, y, w - pad, y + 40.0 * s);
            rt.FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect: sb, radiusX: radius, radiusY: radius }, &b_field);
            rt.FillRectangle(&rf(sb.left + 2.0 * s, sb.bottom - 2.0 * s, sb.right - 2.0 * s, sb.bottom), &b_accent);
            let tr = rf(sb.left + 14.0 * s, sb.top, sb.right - 14.0 * s, sb.bottom);
            if self.query.is_empty() {
                draw_text("Type to search apps, tools and commands", &search_fmt, tr, &b_dim);
            } else {
                let typed_w = Self::text_width(dw, &search_fmt, &self.query);
                draw_text(&self.query, &search_fmt, tr, &b_text);
                let rest = self.completion().map(|c| c.chars().skip(self.query.chars().count()).collect::<String>());
                let caret_x = tr.left + typed_w;
                if let Some(rest) = rest {
                    draw_text(&rest, &search_fmt, rf(caret_x, tr.top, tr.right, tr.bottom), &b_dim);
                    draw_text("Tab", &key_fmt, rf(tr.left, tr.top, tr.right, tr.bottom), &b_dim);
                }
                rt.FillRectangle(&rf(caret_x, sb.top + 10.0 * s, caret_x + 1.0 * s, sb.bottom - 10.0 * s), &b_text);
            }
            y = sb.bottom + 16.0 * s;

            // pinned grid (hidden while searching)
            if self.query.is_empty() {
                draw_text("Pinned", &head_fmt, rf(pad + 4.0 * s, y, w - pad, y + 24.0 * s), &b_text);
                y += 30.0 * s;
                if self.pins.is_empty() {
                    draw_text(
                        "Right-click an app below and choose \"Pin to Start\"",
                        &name_fmt,
                        rf(pad + 4.0 * s, y, w - pad, y + 36.0 * s),
                        &b_dim,
                    );
                    y += 44.0 * s;
                } else {
                    let cols = 6usize;
                    let cell_w = (w - 2.0 * pad) / cols as f32;
                    let cell_h = 84.0 * s;
                    let icon = (40.0 * s).round();
                    for (i, id) in self.pins.clone().iter().enumerate() {
                        let (col, row) = ((i % cols) as f32, (i / cols) as f32);
                        let r = rf(pad + col * cell_w, y + row * cell_h, pad + (col + 1.0) * cell_w, y + (row + 1.0) * cell_h);
                        let hit = StartHit::Pinned(i);
                        if hover_hit == Some(hit) {
                            rt.FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect: r, radiusX: radius, radiusY: radius }, &b_hover);
                        }
                        if let Some(img) = icon_for(&shell_path(id), Self::tile_px(s)) {
                            if let Some(bmp) = cached_bitmap(&mut self.bitmaps, &rt, &img) {
                                used.insert(img.id);
                                let (cx, cy) = ((r.left + r.right) / 2.0, r.top + 10.0 * s + icon / 2.0);
                                let (src, dst) = img.fit(cx, cy, icon);
                                rt.DrawBitmap(&bmp, Some(&dst), 1.0, D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, Some(&src));
                            }
                        }
                        let label = rf(r.left + 4.0 * s, r.top + 54.0 * s, r.right - 4.0 * s, r.bottom - 4.0 * s);
                        draw_text(&self.name_of(id), &tile_fmt, label, &b_text);
                        items.push((hit, r));
                    }
                    let rows = self.pins.len().div_ceil(cols) as f32;
                    y += rows * cell_h + 8.0 * s;
                }
                draw_text("All apps", &head_fmt, rf(pad + 4.0 * s, y, w - pad, y + 24.0 * s), &b_text);
                y += 30.0 * s;
            }

            // footer: account on the left, power on the right, like Windows' Start
            let foot_h = 56.0 * s;
            let foot_top = h - foot_h;
            rt.FillRectangle(&rf(0.0, foot_top, w, h), &b_field);
            let by = foot_top + 8.0 * s;
            let bh = foot_h - 16.0 * s;
            let glyph_w = 32.0 * s;
            let name_w = Self::text_width(dw, &name_fmt, &self.user_name).min(w * 0.5);
            let user_r = rf(pad, by, pad + 8.0 * s + glyph_w + name_w + 14.0 * s, by + bh);
            let power_r = rf(w - pad - bh - 4.0 * s, by, w - pad, by + bh);
            for (hit, r) in [(StartHit::User, user_r), (StartHit::Power, power_r)] {
                if hover_hit == Some(hit) {
                    rt.FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect: r, radiusX: radius, radiusY: radius }, &b_hover);
                }
                items.push((hit, r));
            }
            let gl = user_r.left + 6.0 * s;
            draw_text("\u{E77B}", &glyph_fmt, rf(gl, by, gl + glyph_w, by + bh), &b_text);
            draw_text(&self.user_name, &name_fmt, rf(gl + glyph_w + 4.0 * s, by, user_r.right - 6.0 * s, by + bh), &b_text);
            draw_text("\u{E7E8}", &glyph_fmt, power_r, &b_text);

            // app list, scrollable, clipped to its area
            let list = rf(pad, y, w - pad, foot_top - 8.0 * s);
            let row_h = 40.0 * s;
            self.row_h = row_h;
            let shown = self.filtered();
            self.max_scroll = (shown.len() as f32 * row_h - (list.bottom - list.top)).max(0.0);
            self.scroll = self.scroll.clamp(0.0, self.max_scroll);
            if shown.is_empty() {
                draw_text("Loading apps...", &name_fmt, rf(list.left + 8.0 * s, list.top, list.right, list.top + row_h), &b_dim);
            }
            rt.PushAxisAlignedClip(&list, D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
            let icon = (28.0 * s).round();
            let first = (self.scroll / row_h) as usize;
            let searching = !self.query.trim().is_empty();
            for (k, &hit) in shown.iter().enumerate().skip(first) {
                let top = list.top + k as f32 * row_h - self.scroll;
                if top > list.bottom {
                    break;
                }
                let r = rf(list.left, top, list.right, top + row_h);
                if hover_hit == Some(hit) || (searching && k == 0 && hover_hit.is_none()) {
                    // while searching, the top result is what Enter opens
                    rt.FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect: r, radiusX: radius, radiusY: radius }, &b_hover);
                }
                let (source, label, key) = match hit {
                    StartHit::App(i) => {
                        let a = &self.apps[i];
                        (Some(shell_path(&a.id)), a.name.clone(), a.key.clone())
                    }
                    StartHit::Cmd(i) => {
                        let c = &self.commands[i];
                        let key = if c.extra { "command".to_string() } else { c.key.clone() };
                        (Some(c.path.clone()), c.name.clone(), key)
                    }
                    _ => (None, format!("Run \u{201C}{}\u{201D}", self.query.trim()), "Enter".to_string()),
                };
                match source {
                    Some(src_path) => {
                        if let Some(img) = icon_for(&src_path, Self::row_px(s)) {
                            if let Some(bmp) = cached_bitmap(&mut self.bitmaps, &rt, &img) {
                                used.insert(img.id);
                                let (cx, cy) = (r.left + 8.0 * s + icon / 2.0, (r.top + r.bottom) / 2.0);
                                let (src, dst) = img.fit(cx, cy, icon);
                                rt.DrawBitmap(&bmp, Some(&dst), 1.0, D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, Some(&src));
                            }
                        }
                    }
                    None => draw_text("\u{E756}", &glyph_fmt, rf(r.left + 8.0 * s, r.top, r.left + 8.0 * s + icon, r.bottom), &b_text),
                }
                let key_w = if searching && !key.is_empty() { Self::text_width(dw, &key_fmt, &key) + 12.0 * s } else { 0.0 };
                draw_text(&label, &name_fmt, rf(r.left + 48.0 * s, r.top, r.right - 8.0 * s - key_w, r.bottom), &b_text);
                if key_w > 0.0 {
                    draw_text(&key, &key_fmt, rf(r.left, r.top, r.right - 12.0 * s, r.bottom), &b_dim);
                }
                // hit area limited to the visible part of the row
                let vis = rf(r.left, r.top.max(list.top), r.right, r.bottom.min(list.bottom));
                if vis.bottom > vis.top {
                    items.push((hit, vis));
                }
            }
            rt.PopAxisAlignedClip();

            if rt.EndDraw(None, None).is_err() {
                self.rt = None;
                self.bitmaps.clear();
            }
        }
        self.items = items;
        // keep the uploaded bitmaps while scrolling (re-uploading them as rows come
        // back into view cost time too); only drop them when they pile up
        if self.bitmaps.len() > 800 {
            self.bitmaps.retain(|id, _| used.contains(id));
        }
    }
}

/// Starts building the app list in the background; `notify` gets a message when done.
pub fn refresh_apps_async(notify: HWND, msg: u32) {
    let target = notify.0 as isize;
    std::thread::spawn(move || {
        let mut apps = enumerate_apps();
        let commands = enumerate_commands(&mut apps);
        if let Ok(mut slot) = ENUMERATED.lock() {
            *slot = Some((apps, commands));
        }
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                Some(HWND(target as *mut _)),
                msg,
                windows::Win32::Foundation::WPARAM(0),
                windows::Win32::Foundation::LPARAM(0),
            );
        }
    });
}
