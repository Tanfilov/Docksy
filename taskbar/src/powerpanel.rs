//! Battery panel, opened by the bar's battery icon: battery state, power plan,
//! power mode and the screen-off / sleep timers as one-click choices.
//! Closes when it loses focus.

use std::time::Instant;

use windows::Win32::{
    Foundation::{HWND, RECT},
    Graphics::{
        Direct2D::{
            Common::{D2D_RECT_F, D2D_SIZE_U, D2D1_COLOR_F},
            D2D1_DRAW_TEXT_OPTIONS_CLIP, D2D1_ROUNDED_RECT, ID2D1Factory, ID2D1HwndRenderTarget,
            ID2D1SolidColorBrush,
        },
        DirectWrite::{
            DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT,
            DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_MEASURING_MODE_NATURAL,
            DWRITE_PARAGRAPH_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT, DWRITE_TEXT_ALIGNMENT_CENTER,
            DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_TEXT_ALIGNMENT_TRAILING, DWRITE_TRIMMING,
            DWRITE_TRIMMING_GRANULARITY_CHARACTER, DWRITE_WORD_WRAPPING_NO_WRAP, IDWriteFactory,
            IDWriteTextFormat,
        },
        Gdi::InvalidateRect,
    },
    UI::WindowsAndMessaging::{
        CreateWindowExW, HWND_TOPMOST, SW_HIDE, SWP_SHOWWINDOW, SetWindowPos, ShowWindow,
        WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
    },
};
use windows::core::{PCWSTR, w};

use crate::{
    bar::{ensure_target, rf},
    config::{Config, Position},
    powercfg::{self, Mode, PowerState, Timer},
    util::parse_color,
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PowerHit {
    Plan(usize),
    Mode(Mode),
    /// which timers are shown: plugged in (true) or on battery
    Source(bool),
    Timer(Timer, usize),
    ScreenOff,
    More,
}

/// Static text drawn at a position.
enum Text {
    Title(String, String),
    Label(String, String),
}

pub struct PowerPanel {
    pub hwnd: HWND,
    rt: Option<ID2D1HwndRenderTarget>,
    pub open: bool,
    pub closed_at: Instant,
    pub hover: Option<PowerHit>,
    pub tracking: bool,
    state: PowerState,
    /// timers shown for plugged in (true) or on battery
    ac_tab: bool,
    scale: f32,
    size: (i32, i32),
    anchor: Option<(RECT, RECT, RECT, Position)>,
    items: Vec<(PowerHit, D2D_RECT_F)>,
    texts: Vec<(Text, D2D_RECT_F)>,
}

const W: f32 = 380.0;
const PAD: f32 = 20.0;
const ROW: f32 = 34.0;
const GAP: f32 = 6.0;

impl PowerPanel {
    pub fn new(hinst: windows::Win32::Foundation::HINSTANCE) -> Option<PowerPanel> {
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
                w!("DocksyPower"),
                w!("Docksy power"),
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
        Some(PowerPanel {
            hwnd,
            rt: None,
            open: false,
            closed_at: Instant::now(),
            hover: None,
            tracking: false,
            state: PowerState::default(),
            ac_tab: true,
            scale: 1.0,
            size: (1, 1),
            anchor: None,
            items: Vec::new(),
            texts: Vec::new(),
        })
    }

    pub fn invalidate(&self) {
        unsafe {
            let _ = InvalidateRect(Some(self.hwnd), None, false);
        }
    }

    /// Opens next to the battery button. `button`, `bar` and `monitor` are screen rects.
    pub fn show(&mut self, cfg: &Config, button: RECT, bar: RECT, monitor: RECT, scale: f32) {
        self.scale = scale;
        self.anchor = Some((button, bar, monitor, cfg.position));
        self.state = PowerState::read();
        self.ac_tab = self.state.on_ac;
        crate::backdrop::apply(self.hwnd, cfg, true);
        self.open = true;
        self.relayout();
        crate::util::force_foreground(self.hwnd);
    }

    pub fn hide(&mut self) {
        if !self.open {
            return;
        }
        self.open = false;
        self.closed_at = Instant::now();
        self.hover = None;
        self.tracking = false;
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
    }

    pub fn hit_test(&self, x: i32, y: i32) -> Option<PowerHit> {
        let (x, y) = (x as f32, y as f32);
        self.items
            .iter()
            .find(|(_, r)| x >= r.left && x < r.right && y >= r.top && y < r.bottom)
            .map(|(h, _)| *h)
    }

    /// Runs a click. Returns true when the panel should close (it opened Settings).
    pub fn click(&mut self, hit: PowerHit) -> bool {
        match hit {
            PowerHit::Plan(i) => {
                if let Some((g, _)) = self.state.plans.get(i) {
                    powercfg::set_plan(g);
                }
            }
            PowerHit::Mode(m) => powercfg::set_mode(m),
            PowerHit::Source(ac) => self.ac_tab = ac,
            PowerHit::Timer(t, i) => {
                if let Some(&secs) = t.presets().get(i) {
                    powercfg::set_timer(t, self.ac_tab, secs);
                }
            }
            PowerHit::ScreenOff => {
                crate::power::Power::ScreenOff.run(self.hwnd);
                return true;
            }
            PowerHit::More => {
                crate::util::shell_open("ms-settings:powersleep", None);
                return true;
            }
        }
        self.state = PowerState::read();
        self.relayout();
        false
    }

    /// Lays out the rows for the current state and sizes/positions the window.
    fn relayout(&mut self) {
        let s = self.scale;
        let st = &self.state;
        let w = W * s;
        let (pad, row, gap) = (PAD * s, ROW * s, GAP * s);
        let inner = w - 2.0 * pad;
        let mut items = Vec::new();
        let mut texts = Vec::new();
        let mut y = 14.0 * s;

        // a grid of `cols` equal buttons
        let mut grid = |y: &mut f32, hits: Vec<PowerHit>, cols: usize| {
            let bw = (inner - (cols as f32 - 1.0) * gap) / cols as f32;
            for (k, h) in hits.into_iter().enumerate() {
                let (c, r) = ((k % cols) as f32, (k / cols) as f32);
                let l = pad + c * (bw + gap);
                let t = *y + r * (row + gap);
                items.push((h, rf(l, t, l + bw, t + row)));
            }
        };

        // battery state
        let (title, sub) = match st.battery {
            Some(p) => {
                let state = if st.charging {
                    "Charging".to_string()
                } else if st.on_ac {
                    "Plugged in".to_string()
                } else {
                    match st.time_left {
                        Some(t) => format!("{} h {} min left", t / 3600, (t % 3600) / 60),
                        None => "On battery".to_string(),
                    }
                };
                (format!("{p}%"), state)
            }
            None => ("Power".to_string(), if st.on_ac { "Plugged in" } else { "On battery" }.to_string()),
        };
        texts.push((Text::Title(title, sub), rf(pad, y, w - pad, y + 36.0 * s)));
        y += 48.0 * s;

        // power plan
        if st.plans.len() > 1 {
            let active = st.plans.iter().find(|(g, _)| *g == st.active).map(|(_, n)| n.clone()).unwrap_or_default();
            texts.push((Text::Label("Power plan".into(), active), rf(pad, y, w - pad, y + 22.0 * s)));
            y += 28.0 * s;
            let n = st.plans.len();
            grid(&mut y, (0..n).map(PowerHit::Plan).collect(), 3);
            y += n.div_ceil(3) as f32 * (row + gap) + 10.0 * s;
        }

        // power mode (only on the Balanced plan)
        if let Some(mode) = st.mode {
            texts.push((Text::Label("Power mode".into(), mode.label().into()), rf(pad, y, w - pad, y + 22.0 * s)));
            y += 28.0 * s;
            grid(&mut y, Mode::ALL.iter().map(|m| PowerHit::Mode(*m)).collect(), 3);
            y += row + gap + 10.0 * s;
        }

        // timers, for plugged in or on battery
        if st.battery.is_some() {
            grid(&mut y, vec![PowerHit::Source(true), PowerHit::Source(false)], 2);
            y += row + gap + 8.0 * s;
        }
        for t in [Timer::Screen, Timer::Sleep] {
            let cur = powercfg::duration_label(st.timer(t, self.ac_tab), false);
            texts.push((Text::Label(t.label().into(), cur), rf(pad, y, w - pad, y + 22.0 * s)));
            y += 28.0 * s;
            let n = t.presets().len();
            grid(&mut y, (0..n).map(|i| PowerHit::Timer(t, i)).collect(), 4);
            y += n.div_ceil(4) as f32 * (row + gap) + 10.0 * s;
        }

        // turn off screen + link to Settings
        let half = (inner - gap) / 2.0;
        items.push((PowerHit::ScreenOff, rf(pad, y, pad + half, y + row)));
        items.push((PowerHit::More, rf(pad + half + gap, y, w - pad, y + row)));
        y += row + 14.0 * s;

        self.items = items;
        self.texts = texts;
        let (w, h) = (w.round() as i32, y.round() as i32);
        self.size = (w, h);
        let Some((button, bar, monitor, pos)) = self.anchor else { return };
        let gap = (8.0 * s).round() as i32;
        let (cx, cy) = ((button.left + button.right) / 2, (button.top + button.bottom) / 2);
        let (mut x, mut y) = match pos {
            Position::Bottom => (cx - w / 2, bar.top - gap - h),
            Position::Top => (cx - w / 2, bar.bottom + gap),
            Position::Left => (bar.right + gap, cy - h / 2),
            Position::Right => (bar.left - gap - w, cy - h / 2),
        };
        x = x.clamp(monitor.left + gap, (monitor.right - gap - w).max(monitor.left));
        y = y.clamp(monitor.top + gap, (monitor.bottom - gap - h).max(monitor.top));
        unsafe {
            let _ = SetWindowPos(self.hwnd, Some(HWND_TOPMOST), x, y, w, h, SWP_SHOWWINDOW);
        }
        self.invalidate();
    }

    fn format(dw: &IDWriteFactory, size: f32, weight: DWRITE_FONT_WEIGHT, align: DWRITE_TEXT_ALIGNMENT, family: PCWSTR) -> Option<IDWriteTextFormat> {
        unsafe {
            let f = dw
                .CreateTextFormat(family, None, weight, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_STRETCH_NORMAL, size, w!(""))
                .ok()?;
            let _ = f.SetTextAlignment(align);
            let _ = f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER);
            let _ = f.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP);
            // long plan names end in "…"
            let trim = DWRITE_TRIMMING { granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER, ..Default::default() };
            if let Ok(sign) = dw.CreateEllipsisTrimmingSign(&f) {
                let _ = f.SetTrimming(&trim, &sign);
            }
            Some(f)
        }
    }

    fn label(&self, hit: PowerHit) -> String {
        match hit {
            PowerHit::Plan(i) => self.state.plans.get(i).map(|(_, n)| n.clone()).unwrap_or_default(),
            PowerHit::Mode(m) => m.label().into(),
            PowerHit::Source(true) => "Plugged in".into(),
            PowerHit::Source(false) => "On battery".into(),
            PowerHit::Timer(t, i) => t.presets().get(i).map(|&v| powercfg::duration_label(v, true)).unwrap_or_default(),
            PowerHit::ScreenOff => "Turn off screen".into(),
            PowerHit::More => "Power settings".into(),
        }
    }

    fn selected(&self, hit: PowerHit) -> bool {
        let st = &self.state;
        match hit {
            PowerHit::Plan(i) => st.plans.get(i).is_some_and(|(g, _)| *g == st.active),
            PowerHit::Mode(m) => st.mode == Some(m),
            PowerHit::Source(ac) => self.ac_tab == ac,
            PowerHit::Timer(t, i) => t.presets().get(i) == Some(&st.timer(t, self.ac_tab)),
            PowerHit::ScreenOff | PowerHit::More => false,
        }
    }

    pub fn paint(&mut self, d2d: &ID2D1Factory, dw: &IDWriteFactory, cfg: &Config) {
        if !self.open {
            return;
        }
        let size = D2D_SIZE_U {
            width: self.size.0.max(1) as u32,
            height: self.size.1.max(1) as u32,
        };
        let Some(rt) = ensure_target(&mut self.rt, d2d, self.hwnd, size) else { return };
        let s = self.scale;
        let font = w!("Segoe UI Variable Text");
        let (Some(title_fmt), Some(label_fmt), Some(value_fmt), Some(button_fmt)) = (
            Self::format(dw, 22.0 * s, DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_TEXT_ALIGNMENT_LEADING, font),
            Self::format(dw, 14.0 * s, DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_TEXT_ALIGNMENT_LEADING, font),
            Self::format(dw, 14.0 * s, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_TEXT_ALIGNMENT_TRAILING, font),
            Self::format(dw, 13.0 * s, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_TEXT_ALIGNMENT_CENTER, font),
        ) else {
            return;
        };
        let text = parse_color(&cfg.text);
        let dim = D2D1_COLOR_F { a: text.a * 0.65, ..text };
        let accent = parse_color(&cfg.accent);
        let hover = parse_color(&cfg.hover);
        let button = D2D1_COLOR_F { r: 1.0, g: 1.0, b: 1.0, a: 0.08 };
        let on_accent = D2D1_COLOR_F { r: 0.0, g: 0.0, b: 0.0, a: 0.9 };
        let brush = |c: D2D1_COLOR_F| -> Option<ID2D1SolidColorBrush> { unsafe { rt.CreateSolidColorBrush(&c, None) }.ok() };
        let (Some(b_text), Some(b_dim), Some(b_accent), Some(b_hover), Some(b_button), Some(b_on_accent)) =
            (brush(text), brush(dim), brush(accent), brush(hover), brush(button), brush(on_accent))
        else {
            return;
        };
        let draw = |t: &str, fmt: &IDWriteTextFormat, r: D2D_RECT_F, b: &ID2D1SolidColorBrush| {
            let t: Vec<u16> = t.encode_utf16().collect();
            unsafe { rt.DrawText(&t, fmt, &r, b, D2D1_DRAW_TEXT_OPTIONS_CLIP, DWRITE_MEASURING_MODE_NATURAL) };
        };
        unsafe {
            rt.BeginDraw();
            rt.Clear(Some(&crate::backdrop::clear_color(cfg)));
            for (t, r) in &self.texts {
                match t {
                    Text::Title(big, small) => {
                        draw(big, &title_fmt, *r, &b_text);
                        draw(small, &value_fmt, *r, &b_dim);
                    }
                    Text::Label(l, v) => {
                        draw(l, &label_fmt, *r, &b_text);
                        draw(v, &value_fmt, *r, &b_dim);
                    }
                }
            }
            for (h, r) in &self.items {
                let selected = self.selected(*h);
                let fill = if selected {
                    &b_accent
                } else if self.hover == Some(*h) {
                    &b_hover
                } else {
                    &b_button
                };
                rt.FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect: *r, radiusX: 6.0 * s, radiusY: 6.0 * s }, fill);
                let inset = rf(r.left + 6.0 * s, r.top, r.right - 6.0 * s, r.bottom);
                // dark text on the accent fill, like Windows 11's selected buttons
                draw(&self.label(*h), &button_fmt, inset, if selected { &b_on_accent } else { &b_text });
            }
            if rt.EndDraw(None, None).is_err() {
                self.rt = None;
            }
        }
    }
}
