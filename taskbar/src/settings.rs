//! "Taskbar settings" panel: sliders for the taskbar height, app icon size and tray
//! icon size. Values apply live while dragging and are saved to config.toml when
//! the slider is released.

use windows::Win32::{
    Foundation::{HWND, RECT},
    Graphics::{
        Direct2D::{
            Common::{D2D_RECT_F, D2D_SIZE_U, D2D1_COLOR_F},
            D2D1_DRAW_TEXT_OPTIONS_CLIP, D2D1_ELLIPSE, D2D1_ROUNDED_RECT, ID2D1Factory,
            ID2D1HwndRenderTarget, ID2D1SolidColorBrush,
        },
        DirectWrite::{
            DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT,
            DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_MEASURING_MODE_NATURAL,
            DWRITE_PARAGRAPH_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT, DWRITE_TEXT_ALIGNMENT_CENTER,
            DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_TEXT_ALIGNMENT_TRAILING,
            DWRITE_WORD_WRAPPING_NO_WRAP, IDWriteFactory, IDWriteTextFormat,
        },
        Gdi::InvalidateRect,
    },
    UI::WindowsAndMessaging::{
        CreateWindowExW, HWND_TOPMOST, SW_HIDE, SWP_SHOWWINDOW, SetWindowPos,
        ShowWindow, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
    },
};
use windows::core::{PCWSTR, w};

use crate::{
    bar::{ensure_target, rf},
    config::{Config, Position},
    util::parse_color,
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Slider {
    Height,
    Icon,
    Tray,
}

impl Slider {
    pub const ALL: [Slider; 3] = [Slider::Height, Slider::Icon, Slider::Tray];

    fn label(self) -> &'static str {
        match self {
            Slider::Height => "Taskbar height",
            Slider::Icon => "App icon size",
            Slider::Tray => "Tray icon size",
        }
    }

    /// (min, max) in logical px
    pub fn range(self) -> (f32, f32) {
        match self {
            Slider::Height => (36.0, 160.0),
            Slider::Icon => (16.0, 128.0),
            Slider::Tray => (12.0, 64.0),
        }
    }

    pub fn value(self, cfg: &Config) -> f32 {
        match self {
            Slider::Height => cfg.height,
            Slider::Icon => cfg.icon_size,
            Slider::Tray => cfg.tray_icon_size,
        }
    }

    /// config.toml key
    pub fn key(self) -> &'static str {
        match self {
            Slider::Height => "height",
            Slider::Icon => "icon_size",
            Slider::Tray => "tray_icon_size",
        }
    }
}

/// Clickable parts of the panel.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SettingsHit {
    Slider(Slider),
    Position(Position),
    Reset,
    Close,
}

pub struct SettingsPanel {
    pub hwnd: HWND,
    rt: Option<ID2D1HwndRenderTarget>,
    pub open: bool,
    scale: f32,
    size: (i32, i32),
    /// slider being dragged
    pub dragging: Option<Slider>,
    pub hover: Option<SettingsHit>,
    pub tracking: bool,
    /// (slider, track rect: the thumb's travel, in client px)
    tracks: Vec<(Slider, D2D_RECT_F)>,
    /// slider rows (whole row is clickable)
    rows: Vec<(Slider, D2D_RECT_F)>,
    reset_rect: D2D_RECT_F,
    close_rect: D2D_RECT_F,
    /// taskbar position buttons
    positions: Vec<(Position, D2D_RECT_F)>,
}

impl SettingsPanel {
    pub fn new(hinst: windows::Win32::Foundation::HINSTANCE) -> Option<SettingsPanel> {
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
                w!("DocksySettings"),
                w!("Docksy taskbar settings"),
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
        Some(SettingsPanel {
            hwnd,
            rt: None,
            open: false,
            scale: 1.0,
            size: (1, 1),
            dragging: None,
            hover: None,
            tracking: false,
            tracks: Vec::new(),
            rows: Vec::new(),
            reset_rect: rf(0.0, 0.0, 0.0, 0.0),
            positions: Vec::new(),
            close_rect: rf(0.0, 0.0, 0.0, 0.0),
        })
    }

    pub fn invalidate(&self) {
        unsafe {
            let _ = InvalidateRect(Some(self.hwnd), None, false);
        }
    }

    /// Opens centered on `monitor` (screen rect).
    pub fn show(&mut self, cfg: &Config, monitor: RECT, scale: f32) {
        self.scale = scale;
        let w = (480.0 * scale).round() as i32;
        let h = (446.0 * scale).round() as i32;
        let x = (monitor.left + monitor.right - w) / 2;
        let y = (monitor.top + monitor.bottom - h) / 2;
        self.size = (w, h);
        crate::backdrop::apply(self.hwnd, cfg, true);
        self.open = true;
        unsafe {
            let _ = SetWindowPos(self.hwnd, Some(HWND_TOPMOST), x, y, w, h, SWP_SHOWWINDOW);
            crate::util::force_foreground(self.hwnd);
        }
        self.invalidate();
    }

    pub fn hide(&mut self) {
        if !self.open {
            return;
        }
        self.open = false;
        self.dragging = None;
        self.hover = None;
        self.tracking = false;
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
    }

    pub fn hit_test(&self, x: i32, y: i32) -> Option<SettingsHit> {
        let (x, y) = (x as f32, y as f32);
        let inside = |r: &D2D_RECT_F| x >= r.left && x < r.right && y >= r.top && y < r.bottom;
        if inside(&self.close_rect) {
            return Some(SettingsHit::Close);
        }
        if inside(&self.reset_rect) {
            return Some(SettingsHit::Reset);
        }
        if let Some((p, _)) = self.positions.iter().find(|(_, r)| inside(r)) {
            return Some(SettingsHit::Position(*p));
        }
        self.rows.iter().find(|(_, r)| inside(r)).map(|(s, _)| SettingsHit::Slider(*s))
    }

    /// Slider value for mouse x (client px), rounded to whole logical px.
    pub fn value_at(&self, slider: Slider, x: i32) -> Option<f32> {
        let (_, track) = self.tracks.iter().find(|(s, _)| *s == slider)?;
        let t = ((x as f32 - track.left) / (track.right - track.left)).clamp(0.0, 1.0);
        let (min, max) = slider.range();
        Some((min + t * (max - min)).round())
    }

    fn format(dw: &IDWriteFactory, size: f32, weight: DWRITE_FONT_WEIGHT, align: DWRITE_TEXT_ALIGNMENT, family: PCWSTR) -> Option<IDWriteTextFormat> {
        unsafe {
            let f = dw
                .CreateTextFormat(family, None, weight, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_STRETCH_NORMAL, size, w!(""))
                .ok()?;
            let _ = f.SetTextAlignment(align);
            let _ = f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER);
            let _ = f.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP);
            Some(f)
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
        let (w, _h) = (self.size.0 as f32, self.size.1 as f32);
        let pad = 24.0 * s;
        let font = w!("Segoe UI Variable Text");
        let (Some(title_fmt), Some(label_fmt), Some(value_fmt), Some(button_fmt), Some(glyph_fmt)) = (
            Self::format(dw, 18.0 * s, DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_TEXT_ALIGNMENT_LEADING, font),
            Self::format(dw, 14.0 * s, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_TEXT_ALIGNMENT_LEADING, font),
            Self::format(dw, 14.0 * s, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_TEXT_ALIGNMENT_TRAILING, font),
            Self::format(dw, 14.0 * s, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_TEXT_ALIGNMENT_CENTER, font),
            Self::format(dw, 12.0 * s, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_TEXT_ALIGNMENT_CENTER, w!("Segoe Fluent Icons")),
        ) else {
            return;
        };
        let text = parse_color(&cfg.text);
        let dim = D2D1_COLOR_F { a: text.a * 0.65, ..text };
        let accent = parse_color(&cfg.accent);
        let rail = D2D1_COLOR_F { r: 1.0, g: 1.0, b: 1.0, a: 0.22 };
        let hover = parse_color(&cfg.hover);
        let button = D2D1_COLOR_F { r: 1.0, g: 1.0, b: 1.0, a: 0.08 };
        let ring = D2D1_COLOR_F { r: 0.27, g: 0.27, b: 0.27, a: 1.0 };
        let on_accent = D2D1_COLOR_F { r: 0.0, g: 0.0, b: 0.0, a: 0.9 };
        let brush = |c: D2D1_COLOR_F| -> Option<ID2D1SolidColorBrush> { unsafe { rt.CreateSolidColorBrush(&c, None) }.ok() };
        let (Some(b_text), Some(b_dim), Some(b_accent), Some(b_rail), Some(b_hover), Some(b_button), Some(b_ring)) =
            (brush(text), brush(dim), brush(accent), brush(rail), brush(hover), brush(button), brush(ring))
        else {
            return;
        };
        let Some(b_on_accent) = brush(on_accent) else { return };
        let b_rail_off = brush(D2D1_COLOR_F { a: 0.08, ..rail });
        let draw = |t: &str, fmt: &IDWriteTextFormat, r: D2D_RECT_F, b: &ID2D1SolidColorBrush| {
            let t: Vec<u16> = t.encode_utf16().collect();
            unsafe { rt.DrawText(&t, fmt, &r, b, D2D1_DRAW_TEXT_OPTIONS_CLIP, DWRITE_MEASURING_MODE_NATURAL) };
        };
        let round = |r: D2D_RECT_F, radius: f32| D2D1_ROUNDED_RECT { rect: r, radiusX: radius, radiusY: radius };

        let mut tracks = Vec::new();
        let mut rows = Vec::new();
        unsafe {
            rt.BeginDraw();
            rt.Clear(Some(&crate::backdrop::clear_color(cfg)));

            // title + close
            draw("Taskbar settings", &title_fmt, rf(pad, 14.0 * s, w - 60.0 * s, 54.0 * s), &b_text);
            self.close_rect = rf(w - 52.0 * s, 12.0 * s, w - 12.0 * s, 52.0 * s);
            if self.hover == Some(SettingsHit::Close) {
                rt.FillRoundedRectangle(&round(self.close_rect, 6.0 * s), &b_hover);
            }
            draw("\u{E8BB}", &glyph_fmt, self.close_rect, &b_text);

            // sliders
            let mut y = 70.0 * s;
            for slider in Slider::ALL {
                let value = slider.value(cfg);
                let (min, max) = slider.range();
                draw(slider.label(), &label_fmt, rf(pad, y, w - pad, y + 24.0 * s), &b_text);
                // icons must fit inside the bar with padding: the rest of the rail is off-limits
                let limit = match slider {
                    Slider::Height => None,
                    Slider::Icon | Slider::Tray => Some(crate::config::max_icon_size(cfg.height).floor()),
                };
                let value_text = match limit {
                    Some(l) if l < max => format!("{} px · max {}", value.round(), l),
                    _ => format!("{} px", value.round()),
                };
                draw(&value_text, &value_fmt, rf(pad, y, w - pad, y + 24.0 * s), &b_dim);
                let ty = y + 42.0 * s;
                let thumb_r = 10.0 * s;
                let track = rf(pad + thumb_r, ty, w - pad - thumb_r, ty);
                let t = ((value - min) / (max - min)).clamp(0.0, 1.0);
                let tx = track.left + t * (track.right - track.left);
                let rail_h = 4.0 * s;
                let lx = match limit {
                    Some(l) => track.left + ((l - min) / (max - min)).clamp(0.0, 1.0) * (track.right - track.left),
                    None => track.right,
                };
                rt.FillRoundedRectangle(&round(rf(track.left, ty - rail_h / 2.0, lx, ty + rail_h / 2.0), rail_h / 2.0), &b_rail);
                if lx < track.right {
                    if let Some(b_off) = &b_rail_off {
                        let thin = rail_h / 4.0;
                        rt.FillRectangle(&rf(lx, ty - thin, track.right, ty + thin), b_off);
                    }
                    // end stop at the limit
                    rt.FillRoundedRectangle(&round(rf(lx - 1.0 * s, ty - 7.0 * s, lx + 1.0 * s, ty + 7.0 * s), 1.0 * s), &b_dim);
                }
                rt.FillRoundedRectangle(&round(rf(track.left, ty - rail_h / 2.0, tx, ty + rail_h / 2.0), rail_h / 2.0), &b_accent);
                // thumb: dark ring with an accent dot that grows on hover/drag, like Windows 11
                let active = self.dragging == Some(slider) || self.hover == Some(SettingsHit::Slider(slider));
                let center = point(tx, ty);
                rt.FillEllipse(&D2D1_ELLIPSE { point: center, radiusX: thumb_r, radiusY: thumb_r }, &b_ring);
                let dot = if active { thumb_r * 0.7 } else { thumb_r * 0.5 };
                rt.FillEllipse(&D2D1_ELLIPSE { point: center, radiusX: dot, radiusY: dot }, &b_accent);
                tracks.push((slider, track));
                rows.push((slider, rf(pad, ty - 16.0 * s, w - pad, ty + 16.0 * s)));
                y += 74.0 * s;
            }

            // taskbar position: four segmented buttons, current one highlighted
            draw("Taskbar position", &label_fmt, rf(pad, y, w - pad, y + 24.0 * s), &b_text);
            let by = y + 34.0 * s;
            let gap = 8.0 * s;
            let bw4 = (w - 2.0 * pad - 3.0 * gap) / 4.0;
            let mut positions = Vec::new();
            for (i, p) in Position::ALL.iter().enumerate() {
                let left = pad + i as f32 * (bw4 + gap);
                let r = rf(left, by, left + bw4, by + 36.0 * s);
                let selected = cfg.position == *p;
                let fill = if selected {
                    &b_accent
                } else if self.hover == Some(SettingsHit::Position(*p)) {
                    &b_hover
                } else {
                    &b_button
                };
                rt.FillRoundedRectangle(&round(r, 6.0 * s), fill);
                let label = match p {
                    Position::Bottom => "Bottom",
                    Position::Top => "Top",
                    Position::Left => "Left",
                    Position::Right => "Right",
                };
                // dark text on the accent fill, like Windows 11's selected buttons
                let tb = if selected { &b_on_accent } else { &b_text };
                draw(label, &button_fmt, r, tb);
                positions.push((*p, r));
            }
            self.positions = positions;
            y = by + 36.0 * s + 22.0 * s;

            // reset button
            let bw = 160.0 * s;
            self.reset_rect = rf(pad, y + 4.0 * s, pad + bw, y + 40.0 * s);
            let b = if self.hover == Some(SettingsHit::Reset) { &b_hover } else { &b_button };
            rt.FillRoundedRectangle(&round(self.reset_rect, 6.0 * s), b);
            draw("Reset to defaults", &button_fmt, self.reset_rect, &b_text);

            if rt.EndDraw(None, None).is_err() {
                self.rt = None;
            }
        }
        self.tracks = tracks;
        self.rows = rows;
    }
}

fn point(x: f32, y: f32) -> windows_numerics::Vector2 {
    windows_numerics::Vector2 { X: x, Y: y }
}
