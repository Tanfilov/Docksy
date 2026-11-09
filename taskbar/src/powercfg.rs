//! Power plans, Windows 11's power mode and the screen/sleep timers, for the
//! battery panel. Same calls as Settings > Power & battery (no admin needed).

use windows::Win32::{
    Foundation::{HLOCAL, LocalFree},
    System::{
        LibraryLoader::{GetProcAddress, LoadLibraryW},
        Power::{
            ACCESS_SCHEME, GetSystemPowerStatus, PowerEnumerate, PowerGetActiveScheme,
            PowerReadACValueIndex, PowerReadDCValueIndex, PowerReadFriendlyName, PowerSetActiveScheme,
            PowerWriteACValueIndex, PowerWriteDCValueIndex, SYSTEM_POWER_STATUS,
        },
    },
};
use windows::core::{GUID, s, w};

const BALANCED: GUID = GUID::from_u128(0x381b4222_f694_41f0_9685_ff5bb260df2e);
const SUB_VIDEO: GUID = GUID::from_u128(0x7516b95f_f776_4464_8c53_06167f40cc99);
const VIDEO_IDLE: GUID = GUID::from_u128(0x3c0bc021_c8a8_4e07_a973_6b14cbcb2b7e);
const SUB_SLEEP: GUID = GUID::from_u128(0x238c9fa8_0aad_41ed_83f4_97be242c8f20);
const STANDBY_IDLE: GUID = GUID::from_u128(0x29f6c1db_86da_48c5_9fdb_f2b67b1f44da);

/// Windows 11 power mode: an overlay on the Balanced plan.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Efficiency,
    Balanced,
    Performance,
}

impl Mode {
    pub const ALL: [Mode; 3] = [Mode::Efficiency, Mode::Balanced, Mode::Performance];

    pub fn label(self) -> &'static str {
        match self {
            Mode::Efficiency => "Efficiency",
            Mode::Balanced => "Balanced",
            Mode::Performance => "Performance",
        }
    }

    fn guid(self) -> GUID {
        match self {
            Mode::Efficiency => GUID::from_u128(0x961cc777_2547_4f9d_8174_7d86181b8a7a),
            Mode::Balanced => GUID::zeroed(),
            Mode::Performance => GUID::from_u128(0xded574b5_45a0_4f42_8737_46345c09c238),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Timer {
    Screen,
    Sleep,
}

impl Timer {
    pub fn label(self) -> &'static str {
        match self {
            Timer::Screen => "Turn off screen after",
            Timer::Sleep => "Sleep after",
        }
    }

    /// quick choices, in seconds (0 = never)
    pub fn presets(self) -> &'static [u32] {
        match self {
            Timer::Screen => &[60, 120, 300, 600, 900, 1800, 3600, 0],
            Timer::Sleep => &[300, 600, 900, 1800, 3600, 7200, 18000, 0],
        }
    }

    fn ids(self) -> (GUID, GUID) {
        match self {
            Timer::Screen => (SUB_VIDEO, VIDEO_IDLE),
            Timer::Sleep => (SUB_SLEEP, STANDBY_IDLE),
        }
    }
}

/// "5 min", "2 hours", "Never"
pub fn duration_label(secs: u32, short: bool) -> String {
    match secs {
        0 => "Never".into(),
        s if s % 3600 == 0 => {
            let h = s / 3600;
            if short {
                format!("{h} h")
            } else if h == 1 {
                "1 hour".into()
            } else {
                format!("{h} hours")
            }
        }
        s if s % 60 == 0 => format!("{} min", s / 60),
        s => format!("{s} s"),
    }
}

#[derive(Clone, Default)]
pub struct PowerState {
    /// (plan, name)
    pub plans: Vec<(GUID, String)>,
    pub active: GUID,
    /// power mode; None when the active plan has none (only Balanced does)
    pub mode: Option<Mode>,
    pub on_ac: bool,
    pub battery: Option<u8>,
    pub charging: bool,
    /// seconds of battery left, if Windows knows
    pub time_left: Option<u32>,
    /// [plugged in, on battery] in seconds
    pub screen: [u32; 2],
    pub sleep: [u32; 2],
}

impl PowerState {
    pub fn read() -> PowerState {
        let active = active_scheme().unwrap_or_default();
        let mut st = PowerState {
            plans: plans(),
            active,
            mode: if active == BALANCED { overlay() } else { None },
            ..Default::default()
        };
        let mut ps = SYSTEM_POWER_STATUS::default();
        if unsafe { GetSystemPowerStatus(&mut ps) }.is_ok() {
            st.on_ac = ps.ACLineStatus != 0;
            if ps.BatteryFlag != 128 && ps.BatteryFlag != 255 && ps.BatteryLifePercent <= 100 {
                st.battery = Some(ps.BatteryLifePercent);
                st.charging = ps.BatteryFlag & 8 != 0;
                st.time_left = (ps.BatteryLifeTime != u32::MAX && !st.on_ac).then_some(ps.BatteryLifeTime);
            }
        }
        for (timer, slot) in [(Timer::Screen, &mut st.screen), (Timer::Sleep, &mut st.sleep)] {
            let (sub, set) = timer.ids();
            unsafe {
                let _ = PowerReadACValueIndex(None, Some(&active), Some(&sub), Some(&set), &mut slot[0]);
                let _ = PowerReadDCValueIndex(None, Some(&active), Some(&sub), Some(&set), &mut slot[1]);
            }
        }
        st
    }

    pub fn timer(&self, timer: Timer, ac: bool) -> u32 {
        let v = match timer {
            Timer::Screen => self.screen,
            Timer::Sleep => self.sleep,
        };
        v[if ac { 0 } else { 1 }]
    }
}

fn active_scheme() -> Option<GUID> {
    unsafe {
        let mut p: *mut GUID = std::ptr::null_mut();
        if PowerGetActiveScheme(None, &mut p).is_err() || p.is_null() {
            return None;
        }
        let g = *p;
        let _ = LocalFree(Some(HLOCAL(p as _)));
        Some(g)
    }
}

fn plans() -> Vec<(GUID, String)> {
    let mut out = Vec::new();
    for i in 0.. {
        let mut g = GUID::zeroed();
        let mut size = std::mem::size_of::<GUID>() as u32;
        let r = unsafe { PowerEnumerate(None, None, None, ACCESS_SCHEME, i, Some(&mut g as *mut GUID as *mut u8), &mut size) };
        if r.is_err() {
            break;
        }
        out.push((g, friendly_name(&g)));
    }
    out
}

fn friendly_name(g: &GUID) -> String {
    let mut buf = [0u16; 128];
    let mut size = std::mem::size_of_val(&buf) as u32;
    let r = unsafe { PowerReadFriendlyName(None, Some(g), None, None, Some(buf.as_mut_ptr() as *mut u8), &mut size) };
    if r.is_err() {
        return format!("{g:?}");
    }
    crate::util::from_wide(&buf)
}

type GetOverlay = unsafe extern "system" fn(*mut GUID) -> u32;
// declared as taking the GUID by value, but it takes a pointer: on x64 that's
// the same thing, on ARM64 passing the value crashes inside powrprof
type SetOverlay = unsafe extern "system" fn(*const GUID) -> u32;

/// powrprof's power mode calls (exported, not in the SDK headers).
fn overlay_fn(name: windows::core::PCSTR) -> Option<unsafe extern "system" fn() -> isize> {
    unsafe { GetProcAddress(LoadLibraryW(w!("powrprof.dll")).ok()?, name) }
}

fn overlay() -> Option<Mode> {
    let f: GetOverlay = unsafe { std::mem::transmute(overlay_fn(s!("PowerGetEffectiveOverlayScheme"))?) };
    let mut g = GUID::zeroed();
    if unsafe { f(&mut g) } != 0 {
        return None;
    }
    Some(Mode::ALL.into_iter().find(|m| m.guid() == g).unwrap_or(Mode::Balanced))
}

pub fn set_mode(mode: Mode) {
    let Some(f) = overlay_fn(s!("PowerSetActiveOverlayScheme")) else { return };
    let f: SetOverlay = unsafe { std::mem::transmute(f) };
    let g = mode.guid();
    let r = unsafe { f(&g) };
    crate::log!("power mode {mode:?}: {r}");
}

pub fn set_plan(plan: &GUID) {
    let r = unsafe { PowerSetActiveScheme(None, Some(plan)) };
    crate::log!("power plan {plan:?}: {r:?}");
}

/// Sets a timer of the active plan, for plugged in (`ac`) or on battery.
pub fn set_timer(timer: Timer, ac: bool, secs: u32) {
    let Some(plan) = active_scheme() else { return };
    let (sub, set) = timer.ids();
    let r = unsafe {
        if ac {
            PowerWriteACValueIndex(None, &plan, Some(&sub), Some(&set), secs).0
        } else {
            PowerWriteDCValueIndex(None, &plan, Some(&sub), Some(&set), secs)
        }
    };
    // re-activating the plan applies the change
    let r2 = unsafe { PowerSetActiveScheme(None, Some(&plan)) };
    crate::log!("{timer:?} ac={ac} {secs}s: {r:?} {r2:?}");
}
