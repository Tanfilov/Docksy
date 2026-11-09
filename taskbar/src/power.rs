//! Power and session actions for the Start panel and the Start button's menu:
//! lock, sign out, sleep, shut down, restart.

use windows::Win32::{
    Foundation::{HWND, LPARAM, WPARAM},
    System::{
        Power::{GetPwrCapabilities, SYSTEM_POWER_CAPABILITIES, SetSuspendState},
        Shutdown::{EWX_LOGOFF, ExitWindowsEx, LockWorkStation, SHUTDOWN_REASON},
    },
    UI::WindowsAndMessaging::{SC_MONITORPOWER, SendMessageW, WM_SYSCOMMAND},
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Power {
    Lock,
    SignOut,
    Sleep,
    Shutdown,
    Restart,
}

impl Power {
    pub fn label(self) -> &'static str {
        match self {
            Power::Lock => "Lock",
            Power::SignOut => "Sign out",
            Power::Sleep => "Sleep",
            Power::Shutdown => "Shut down",
            Power::Restart => "Restart",
        }
    }

    /// `hwnd`: one of our windows (its default window procedure turns the screen off).
    pub fn run(self, hwnd: HWND) {
        crate::log!("power: {:?}", self);
        unsafe {
            match self {
                Power::Lock => {
                    let _ = LockWorkStation();
                }
                Power::SignOut => {
                    let _ = ExitWindowsEx(EWX_LOGOFF, SHUTDOWN_REASON(0));
                }
                Power::Sleep => {
                    let mut caps = SYSTEM_POWER_CAPABILITIES::default();
                    if GetPwrCapabilities(&mut caps) && caps.AoAc {
                        // Modern Standby PCs (no S3 sleep): turning the screen off is
                        // how they go to sleep; SetSuspendState would hibernate instead
                        SendMessageW(hwnd, WM_SYSCOMMAND, Some(WPARAM(SC_MONITORPOWER as usize)), Some(LPARAM(2)));
                    } else {
                        let _ = SetSuspendState(false, false, false);
                    }
                }
                // shutdown.exe holds the needed privilege handling; /hybrid keeps fast
                // startup, like Start's "Shut down"
                Power::Shutdown => run_hidden("shutdown.exe", &["/s", "/hybrid", "/t", "0"]),
                Power::Restart => run_hidden("shutdown.exe", &["/r", "/t", "0"]),
            }
        }
    }
}

fn run_hidden(exe: &str, args: &[&str]) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    if let Err(e) = std::process::Command::new(exe).args(args).creation_flags(CREATE_NO_WINDOW).spawn() {
        crate::log!("power: {exe} failed: {e}");
    }
}
