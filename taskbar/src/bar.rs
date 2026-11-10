//! One taskbar window per monitor: layout, Direct2D drawing and tooltips.

use std::{
    collections::{HashMap, HashSet},
    hash::{Hash, Hasher},
};

use windows::Win32::{
    Foundation::{HWND, RECT},
    Graphics::{
        Direct2D::{
            Common::{D2D_RECT_F, D2D_SIZE_U, D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT},
            D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, D2D1_BITMAP_PROPERTIES,
            D2D1_DRAW_TEXT_OPTIONS_NONE, D2D1_HWND_RENDER_TARGET_PROPERTIES,
            D2D1_PRESENT_OPTIONS_NONE, D2D1_RENDER_TARGET_PROPERTIES, D2D1_ROUNDED_RECT,
            ID2D1Bitmap, ID2D1Factory, ID2D1HwndRenderTarget, ID2D1SolidColorBrush,
        },
        DirectWrite::{
            DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT_NORMAL,
            DWRITE_MEASURING_MODE_NATURAL, DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
            DWRITE_TEXT_ALIGNMENT_CENTER, DWRITE_TEXT_METRICS, IDWriteFactory, IDWriteTextFormat,
        },
        Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
    },
    UI::{
        Controls::{TOOLTIPS_CLASSW, TTF_SUBCLASS, TTM_ADDTOOLW, TTM_DELTOOLW, TTM_SETMAXTIPWIDTH, TTS_ALWAYSTIP, TTS_NOPREFIX, TTTOOLINFOW},
        WindowsAndMessaging::{CreateWindowExW, SendMessageW, WINDOW_STYLE, WS_EX_TOPMOST, WS_POPUP},
    },
};
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::core::{PCWSTR, PWSTR, w};

use crate::{
    apps::Group,
    config::{Config, Position},
    icons::{IconCache, Image},
    tray::Tray,
    util::{self, parse_color},
};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Hit {
    Start,
    App(usize),
    Tray(usize),
    Quick,
    Clock,
    Desktop,
    /// ^ button that opens the tray overflow flyout
    Overflow,
    Battery,
    /// CPU / GPU / RAM monitor
    Monitor,
    /// input language indicator
    Lang,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Battery {
    pub percent: u8,
    pub plugged_in: bool,
    pub charging: bool,
}

impl Battery {
    /// Segoe Fluent Icons battery glyph for the charge level.
    fn glyph(&self) -> String {
        let p = self.percent as u32;
        let code = if self.plugged_in {
            0xE85A + (p / 10).min(10) // BatteryCharging0..10
        } else if p >= 100 {
            0xE83F // Battery10
        } else {
            0xE850 + p / 10 // Battery0..9
        };
        char::from_u32(code).map(String::from).unwrap_or_default()
    }

    fn tip(&self) -> String {
        let state = if self.charging {
            "charging"
        } else if self.plugged_in {
            "plugged in"
        } else {
            "remaining"
        };
        format!("Battery: {}% {state}", self.percent)
    }
}

pub struct Bar {
    pub hwnd: HWND,
    pub mon_rect: RECT,
    pub primary: bool,
    pub scale: f32,
    pub rect: RECT,
    rt: Option<ID2D1HwndRenderTarget>,
    bitmaps: HashMap<u64, ID2D1Bitmap>,
    pub items: Vec<(Hit, D2D_RECT_F)>,
    pub hover: Option<Hit>,
    pub pressed: Option<Hit>,
    pub tracking: bool,
    pub swallow_up: bool,
    tooltip: HWND,
    tips_sig: u64,
    tips_count: usize,
}

pub struct PaintCtx<'a> {
    pub cfg: &'a Config,
    pub groups: &'a [Group],
    pub tray: &'a Tray,
    pub flashing: &'a HashSet<isize>,
    pub icons: &'a mut IconCache,
    pub d2d: &'a ID2D1Factory,
    pub dw: &'a IDWriteFactory,
    pub fg: HWND,
    pub time: &'a str,
    pub date: &'a str,
    pub long_date: &'a str,
    pub quick: &'a str,
    pub overflow_open: bool,
    pub battery: Option<Battery>,
    /// a size slider is being dragged: scale loaded icons instead of loading new sizes
    pub fast_icons: bool,
    /// system monitor values (None = hidden)
    pub stats: Option<crate::monitor::Stats>,
    /// input language (None = hidden)
    pub lang: Option<&'a crate::lang::InputLang>,
}

/// Monitor rows: (label, value, hot). "Hot" (shown in the attention color) means
/// load >= 90 % or temperature >= 85 °C.
fn monitor_lines(stats: Option<crate::monitor::Stats>) -> Vec<(&'static str, String, bool)> {
    let Some(st) = stats else { return Vec::new() };
    let value = |load: Option<f32>, temp: Option<f32>| {
        let mut v = load.map(|l| format!("{l:.0}%")).unwrap_or_else(|| "–".into());
        if let Some(t) = temp {
            v.push_str(&format!(" {t:.0}°"));
        }
        v
    };
    let hot = |load: Option<f32>, temp: Option<f32>| load.is_some_and(|l| l >= 90.0) || temp.is_some_and(|t| t >= 85.0);
    let mut rows = vec![("CPU", value(st.cpu, st.cpu_temp), hot(st.cpu, st.cpu_temp))];
    if st.gpu.is_some() || st.gpu_temp.is_some() {
        rows.push(("GPU", value(st.gpu, st.gpu_temp), hot(st.gpu, st.gpu_temp)));
    }
    rows.push(("RAM", value(st.ram, None), hot(st.ram, None)));
    rows
}

/// Uploads an icon image to a render target once, reusing it afterwards.
pub fn cached_bitmap(
    cache: &mut HashMap<u64, ID2D1Bitmap>,
    rt: &ID2D1HwndRenderTarget,
    img: &Image,
) -> Option<ID2D1Bitmap> {
    if let Some(b) = cache.get(&img.id) {
        return Some(b.clone());
    }
    let props = D2D1_BITMAP_PROPERTIES {
        pixelFormat: D2D1_PIXEL_FORMAT {
            format: DXGI_FORMAT_B8G8R8A8_UNORM,
            alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
        },
        dpiX: 96.0,
        dpiY: 96.0,
    };
    let b = unsafe {
        rt.CreateBitmap(
            D2D_SIZE_U { width: img.w, height: img.h },
            Some(img.px.as_ptr() as *const _),
            img.w * 4,
            &props,
        )
    }
    .ok()?;
    cache.insert(img.id, b.clone());
    Some(b)
}

/// Returns the window's render target, creating it or resizing it to `size`.
pub fn ensure_target(
    slot: &mut Option<ID2D1HwndRenderTarget>,
    d2d: &ID2D1Factory,
    hwnd: HWND,
    size: D2D_SIZE_U,
) -> Option<ID2D1HwndRenderTarget> {
    if let Some(rt) = slot {
        unsafe {
            if rt.GetPixelSize() != size {
                let _ = rt.Resize(&size);
            }
        }
        return Some(rt.clone());
    }
    let props = D2D1_RENDER_TARGET_PROPERTIES {
        pixelFormat: D2D1_PIXEL_FORMAT {
            format: DXGI_FORMAT_B8G8R8A8_UNORM,
            alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
        },
        dpiX: 96.0,
        dpiY: 96.0,
        ..Default::default()
    };
    let hprops = D2D1_HWND_RENDER_TARGET_PROPERTIES {
        hwnd,
        pixelSize: size,
        presentOptions: D2D1_PRESENT_OPTIONS_NONE,
    };
    let rt = unsafe { d2d.CreateHwndRenderTarget(&props, &hprops) }.ok()?;
    *slot = Some(rt.clone());
    Some(rt)
}

pub fn rf(l: f32, t: f32, r: f32, b: f32) -> D2D_RECT_F {
    D2D_RECT_F { left: l, top: t, right: r, bottom: b }
}

fn contains(r: &D2D_RECT_F, x: f32, y: f32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

impl Bar {
    pub fn new(hwnd: HWND, mon_rect: RECT, primary: bool, scale: f32, rect: RECT) -> Bar {
        let tooltip = unsafe {
            CreateWindowExW(
                WS_EX_TOPMOST,
                TOOLTIPS_CLASSW,
                PCWSTR::null(),
                WS_POPUP | WINDOW_STYLE(TTS_ALWAYSTIP | TTS_NOPREFIX),
                0,
                0,
                0,
                0,
                Some(hwnd),
                None,
                None,
                None,
            )
            .unwrap_or_default()
        };
        unsafe { SendMessageW(tooltip, TTM_SETMAXTIPWIDTH, Some(WPARAM(0)), Some(LPARAM(480))) };
        Bar {
            hwnd,
            mon_rect,
            primary,
            scale,
            rect,
            rt: None,
            bitmaps: HashMap::new(),
            items: Vec::new(),
            hover: None,
            pressed: None,
            tracking: false,
            swallow_up: false,
            tooltip,
            tips_sig: 0,
            tips_count: 0,
        }
    }

    pub fn hit_test(&self, x: i32, y: i32) -> Option<Hit> {
        self.items
            .iter()
            .find(|(_, r)| contains(r, x as f32, y as f32))
            .map(|(h, _)| *h)
    }

    pub fn reset_target(&mut self) {
        self.rt = None;
        self.bitmaps.clear();
    }

    fn width(&self) -> f32 {
        (self.rect.right - self.rect.left) as f32
    }
    fn height(&self) -> f32 {
        (self.rect.bottom - self.rect.top) as f32
    }

    fn text_format(dw: &IDWriteFactory, family: PCWSTR, size: f32) -> Option<IDWriteTextFormat> {
        unsafe {
            let f = dw
                .CreateTextFormat(
                    family,
                    None,
                    DWRITE_FONT_WEIGHT_NORMAL,
                    DWRITE_FONT_STYLE_NORMAL,
                    DWRITE_FONT_STRETCH_NORMAL,
                    size,
                    w!(""),
                )
                .ok()?;
            let _ = f.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER);
            let _ = f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER);
            // bar labels never wrap (explicit "\n" still breaks lines)
            let _ = f.SetWordWrapping(windows::Win32::Graphics::DirectWrite::DWRITE_WORD_WRAPPING_NO_WRAP);
            Some(f)
        }
    }

    /// (width, height) of `text` laid out without wrapping.
    fn measure(dw: &IDWriteFactory, fmt: &IDWriteTextFormat, text: &str) -> (f32, f32) {
        let t: Vec<u16> = text.encode_utf16().collect();
        unsafe {
            let Ok(layout) = dw.CreateTextLayout(&t, fmt, 10000.0, 10000.0) else {
                return (0.0, 0.0);
            };
            let mut m = DWRITE_TEXT_METRICS::default();
            if layout.GetMetrics(&mut m).is_err() {
                return (0.0, 0.0);
            }
            (m.widthIncludingTrailingWhitespace, m.height)
        }
    }

    /// Computes item rects (physical px, client coordinates).
    ///
    /// Works along the bar's "main axis" (x for top/bottom bars, y for left/right
    /// bars): Start + apps from the start (or centered), then tray, quick settings,
    /// clock and the show-desktop strip packed against the far end.
    /// `clock_len`/`quick_len` are those items' text extents along the main axis.
    fn layout(
        &self,
        ctx: &PaintCtx,
        clock_len: f32,
        quick_len: f32,
        tray: &[usize],
        has_overflow: bool,
        battery_len: Option<f32>,
        monitor_len: Option<f32>,
        lang_len: Option<f32>,
    ) -> Vec<(Hit, D2D_RECT_F)> {
        let c = ctx.cfg;
        let s = self.scale;
        let vertical = c.position.is_vertical();
        let (len, thick) = if vertical {
            (self.height(), self.width())
        } else {
            (self.width(), self.height())
        };
        let pad = 4.0 * s;
        let (c0, c1) = (pad, thick - pad);
        // main-axis span + cross-axis span -> rect
        let mk = |a0: f32, a1: f32, b0: f32, b1: f32| if vertical { rf(b0, a0, b1, a1) } else { rf(a0, b0, a1, b1) };
        let mut items = Vec::new();

        // far end, packed inwards
        let mut a = len;
        // show-desktop strip in the screen corner: the far (right) end of a top/bottom
        // bar, the top end of a left/right bar
        let mut start = 0.0;
        if c.show_desktop_button {
            let dw = 10.0 * s;
            if vertical {
                items.push((Hit::Desktop, mk(0.0, dw, 0.0, thick)));
                start = dw;
            } else {
                items.push((Hit::Desktop, mk(a - dw, a, 0.0, thick)));
                a -= dw;
            }
        }
        a -= 4.0 * s;
        let show_tray = self.primary || c.tray_on_all_monitors;
        if show_tray || self.primary {
            let cl = clock_len + if vertical { 16.0 } else { 20.0 } * s;
            items.push((Hit::Clock, mk(a - cl, a, c0, c1)));
            a -= cl;
        }
        if let (Some(ml), true) = (monitor_len, show_tray) {
            let ml = ml + if vertical { 16.0 } else { 20.0 } * s;
            items.push((Hit::Monitor, mk(a - ml, a, c0, c1)));
            a -= ml;
        }
        if let (Some(bl), true) = (battery_len, show_tray) {
            let bl = bl + if vertical { 16.0 } else { 20.0 } * s;
            items.push((Hit::Battery, mk(a - bl, a, c0, c1)));
            a -= bl;
        }
        if c.show_quick_settings && show_tray {
            let ql = quick_len + if vertical { 16.0 } else { 20.0 } * s;
            items.push((Hit::Quick, mk(a - ql, a, c0, c1)));
            a -= ql;
        }
        if let (Some(ll), true) = (lang_len, show_tray) {
            let ll = ll + if vertical { 12.0 } else { 16.0 } * s;
            items.push((Hit::Lang, mk(a - ll, a, c0, c1)));
            a -= ll;
        }
        if show_tray {
            let tl = (c.tray_icon_size + 14.0) * s;
            for &i in tray.iter().rev() {
                items.push((Hit::Tray(i), mk(a - tl, a, c0, c1)));
                a -= tl;
            }
            // ^ button for the icons hidden in the overflow flyout
            if has_overflow {
                let ol = (c.tray_icon_size + 8.0) * s;
                items.push((Hit::Overflow, mk(a - ol, a, c0, c1)));
                a -= ol;
            }
        }
        // keep the apps clear of the tray area: centered when they fit, otherwise
        // shifted toward the free start of the bar, shrinking only when that is full
        let far_limit = a - c.tray_gap.max(8.0) * s;

        // app strip: [start][apps...]
        let mut strip: Vec<Hit> = Vec::new();
        if c.show_start_button {
            strip.push(Hit::Start);
        }
        strip.extend((0..ctx.groups.len()).map(Hit::App));
        let gap = c.item_spacing * s;
        let mut cell = (c.icon_size + 20.0).max(c.icon_size * 1.4) * s;
        let n = strip.len() as f32;
        if vertical {
            // cells can't be taller than needed when the bar is thin
            cell = cell.min((c.icon_size + 20.0) * s);
        }
        let avail = (far_limit - start - 8.0 * s).max(0.0);
        let mut strip_len = n * cell + (n - 1.0).max(0.0) * gap;
        if strip_len > avail && n > 0.0 {
            cell = ((avail - (n - 1.0) * gap) / n).max(8.0 * s);
            strip_len = n * cell + (n - 1.0).max(0.0) * gap;
        }
        let mut a0 = if c.center_apps { (len - strip_len) / 2.0 } else { start + 8.0 * s };
        a0 = a0.min(far_limit - strip_len).max(start + 8.0 * s);
        for hit in strip {
            items.push((hit, mk(a0, a0 + cell, c0, c1)));
            a0 += cell + gap;
        }
        items
    }

    fn bitmap(&mut self, rt: &ID2D1HwndRenderTarget, img: &Image) -> Option<ID2D1Bitmap> {
        cached_bitmap(&mut self.bitmaps, rt, img)
    }

    fn ensure_target(&mut self, d2d: &ID2D1Factory) -> Option<ID2D1HwndRenderTarget> {
        let size = D2D_SIZE_U {
            width: self.width().max(1.0) as u32,
            height: self.height().max(1.0) as u32,
        };
        ensure_target(&mut self.rt, d2d, self.hwnd, size)
    }

    pub fn paint(&mut self, ctx: &mut PaintCtx) {
        let Some(rt) = self.ensure_target(ctx.d2d) else { return };
        let c = ctx.cfg;
        let s = self.scale;

        let text_fmt = Self::text_format(ctx.dw, w!("Segoe UI Variable Text"), c.font_size * s);
        let small_fmt = Self::text_format(ctx.dw, w!("Segoe UI Variable Text"), c.font_size * 0.88 * s);
        let glyph_fmt = Self::text_format(ctx.dw, w!("Segoe Fluent Icons"), (c.font_size + 2.0) * s);
        let (Some(text_fmt), Some(small_fmt), Some(glyph_fmt)) = (text_fmt, small_fmt, glyph_fmt) else {
            return;
        };

        let vertical = c.position.is_vertical();
        let (time_w, _) = Self::measure(ctx.dw, &text_fmt, ctx.time);
        let (date_w, _) = Self::measure(ctx.dw, &small_fmt, ctx.date);
        let line = c.font_size * 1.35 * s;
        // on a left/right bar the date only shows if it fits the bar's width
        let two_lines = c.show_date
            && !ctx.date.is_empty()
            && if vertical { date_w <= self.width() - 8.0 * s } else { c.height >= 40.0 };
        // quick-settings glyphs are stacked on a left/right bar
        let quick_text: String = if vertical {
            ctx.quick.split_whitespace().collect::<Vec<_>>().join("\n")
        } else {
            ctx.quick.to_string()
        };
        let (quick_w, quick_h) = Self::measure(ctx.dw, &glyph_fmt, &quick_text);
        let (clock_len, quick_len) = if vertical {
            (line * if two_lines { 2.0 } else { 1.0 }, quick_h)
        } else {
            (time_w.max(if two_lines { date_w } else { 0.0 }), quick_w)
        };
        let tray_idx = ctx.tray.shown(c.tray_show_all);
        let has_overflow = !ctx.tray.overflow(c.tray_show_all).is_empty();
        // battery: glyph + "79%", side by side (or stacked on a left/right bar)
        let battery = if c.show_battery { ctx.battery } else { None };
        let bat_glyph = battery.map(|b| b.glyph()).unwrap_or_default();
        let bat_text = battery.map(|b| format!("{}%", b.percent)).unwrap_or_default();
        let (bat_gw, bat_gh) = Self::measure(ctx.dw, &glyph_fmt, &bat_glyph);
        let (bat_tw, bat_th) = Self::measure(ctx.dw, &small_fmt, &bat_text);
        let bat_gap = 4.0 * s;
        let battery_len = battery.map(|_| if vertical { bat_gh + bat_th } else { bat_gw + bat_gap + bat_tw });
        let mon = monitor_lines(ctx.stats);
        // on a left/right bar the lines ("CPU 12% 53°") must fit the bar's width:
        // shrink the text when they don't
        let mon_fmt_owned = if vertical && !mon.is_empty() {
            let widest = mon
                .iter()
                .map(|(l, v, _)| Self::measure(ctx.dw, &small_fmt, &format!("{l} {v}")).0)
                .fold(0.0, f32::max);
            let avail = self.width() - 10.0 * s;
            if widest > avail {
                let k = (avail / widest).max(0.6);
                Self::text_format(ctx.dw, w!("Segoe UI Variable Text"), c.font_size * 0.88 * k * s)
            } else {
                None
            }
        } else {
            None
        };
        let mon_fmt = mon_fmt_owned.as_ref().unwrap_or(&small_fmt);
        let mon_line_h = c.font_size * 0.88 * 1.4 * s;
        let monitor_len = if mon.is_empty() {
            None
        } else if vertical {
            Some(mon.len() as f32 * mon_line_h)
        } else {
            // columns: label above value
            let cols: f32 = mon
                .iter()
                .map(|(l, v, _)| Self::measure(ctx.dw, mon_fmt, l).0.max(Self::measure(ctx.dw, mon_fmt, v).0))
                .sum();
            Some(cols + (mon.len() as f32 - 1.0) * 12.0 * s)
        };
        // input language: just "ENG"
        let lang = ctx.lang.cloned();
        let lang_len = lang.as_ref().map(|l| {
            let (w, h) = Self::measure(ctx.dw, &small_fmt, &l.short);
            if vertical { h } else { w }
        });
        self.items =
            self.layout(ctx, clock_len, quick_len, &tray_idx, has_overflow, battery_len, monitor_len, lang_len);

        let _bg = parse_color(&c.background);
        let hover = parse_color(&c.hover);
        let accent = parse_color(&c.accent);
        let attention = parse_color(&c.attention);
        let text = parse_color(&c.text);
        let brush = |col: D2D1_COLOR_F| -> Option<ID2D1SolidColorBrush> {
            unsafe { rt.CreateSolidColorBrush(&col, None) }.ok()
        };
        let (Some(b_hover), Some(b_accent), Some(b_attn), Some(b_text)) =
            (brush(hover), brush(accent), brush(attention), brush(text))
        else {
            return;
        };
        let mut pressed_hover = hover;
        pressed_hover.a = (hover.a * 0.6).min(1.0);
        let b_soft = brush(pressed_hover);
        let b_attn_bg = brush(D2D1_COLOR_F { a: 0.35, ..attention });

        let fg_key = ctx.fg.0 as isize;
        let mut used: HashSet<u64> = HashSet::new();
        let items = self.items.clone();
        let radius = 6.0 * s;

        unsafe {
            rt.BeginDraw();
            rt.Clear(Some(&crate::backdrop::clear_color(c)));

            for (hit, r) in &items {
                let rr = D2D1_ROUNDED_RECT { rect: *r, radiusX: radius, radiusY: radius };
                let hovered = self.hover == Some(*hit);
                match hit {
                    Hit::Desktop => {
                        // thin separator line across the bar
                        let line = if vertical {
                            // (strip at the top: line on its lower edge)
                            let y = r.bottom - 1.0 * s;
                            rf(r.left + 12.0 * s, y, r.right - 12.0 * s, y + 1.0 * s)
                        } else {
                            let x = r.left + 0.5;
                            rf(x, r.top + 12.0 * s, x + 1.0 * s, r.bottom - 12.0 * s)
                        };
                        rt.FillRectangle(&line, &b_hover);
                        if hovered {
                            rt.FillRectangle(r, &b_hover);
                        }
                    }
                    Hit::Start => {
                        if hovered {
                            rt.FillRoundedRectangle(&rr, &b_hover);
                        }
                        // four squares, same size and placement as the app icons
                        let size = (c.icon_size * s).round();
                        let (cx, cy) = ((r.left + r.right) / 2.0, (r.top + r.bottom) / 2.0 - 1.0 * s);
                        let g = size * 0.06;
                        let q = (size - g) / 2.0;
                        let (x0, y0) = (cx - size / 2.0, cy - size / 2.0);
                        for (dx, dy) in [(0.0, 0.0), (q + g, 0.0), (0.0, q + g), (q + g, q + g)] {
                            rt.FillRectangle(&rf(x0 + dx, y0 + dy, x0 + dx + q, y0 + dy + q), &b_accent);
                        }
                    }
                    Hit::App(i) => {
                        let Some(grp) = ctx.groups.get(*i) else { continue };
                        let focused = grp.windows.iter().any(|w| w.hwnd.0 as isize == fg_key);
                        let flashing = grp.windows.iter().any(|w| ctx.flashing.contains(&(w.hwnd.0 as isize)));
                        if hovered {
                            rt.FillRoundedRectangle(&rr, &b_hover);
                        } else if flashing && !focused {
                            if let Some(b) = &b_attn_bg {
                                rt.FillRoundedRectangle(&rr, b);
                            }
                        } else if focused {
                            if let Some(b) = &b_soft {
                                rt.FillRoundedRectangle(&rr, b);
                            }
                        }
                        let px = (c.icon_size * s).round() as u32;
                        let first = grp.windows.first().map(|w| w.hwnd);
                        // loaded a bit larger than drawn: the visible artwork is scaled to
                        // fill the square (see Image::fit), and stays sharp that way
                        let load_px = (px as f32 * 1.25).round() as u32;
                        let img = if ctx.fast_icons { ctx.icons.nearest(&grp.icon_sources) } else { None }
                            .or_else(|| ctx.icons.get(&grp.icon_sources, load_px, first));
                        if let Some(img) = img {
                            if let Some(bmp) = self.bitmap(&rt, &img) {
                                used.insert(img.id);
                                let (cx, cy) = ((r.left + r.right) / 2.0, (r.top + r.bottom) / 2.0 - 1.0 * s);
                                let (src, dst) = img.fit(cx, cy, px as f32);
                                rt.DrawBitmap(&bmp, Some(&dst), 1.0, D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, Some(&src));
                            }
                        }
                        if !grp.windows.is_empty() {
                            let iw = if focused { 18.0 * s } else { 7.0 * s };
                            let ih = 3.0 * s;
                            // pill on the bar's screen-edge side, like Windows does
                            let (cx, cy) = ((r.left + r.right) / 2.0, (r.top + r.bottom) / 2.0);
                            let rect = match c.position {
                                Position::Top => rf(cx - iw / 2.0, r.top + 1.0 * s, cx + iw / 2.0, r.top + 1.0 * s + ih),
                                Position::Bottom => rf(cx - iw / 2.0, r.bottom - ih - 1.0 * s, cx + iw / 2.0, r.bottom - 1.0 * s),
                                Position::Left => rf(r.left + 1.0 * s, cy - iw / 2.0, r.left + 1.0 * s + ih, cy + iw / 2.0),
                                Position::Right => rf(r.right - ih - 1.0 * s, cy - iw / 2.0, r.right - 1.0 * s, cy + iw / 2.0),
                            };
                            let ind = D2D1_ROUNDED_RECT { rect, radiusX: ih / 2.0, radiusY: ih / 2.0 };
                            let b = if flashing { &b_attn } else if focused { &b_accent } else { &b_text };
                            rt.FillRoundedRectangle(&ind, b);
                        }
                    }
                    Hit::Tray(i) => {
                        if hovered {
                            rt.FillRoundedRectangle(&rr, &b_hover);
                        }
                        let Some(img) = ctx.tray.icons.get(*i).and_then(|t| t.image.clone()) else { continue };
                        if let Some(bmp) = self.bitmap(&rt, &img) {
                            used.insert(img.id);
                            let sz = c.tray_icon_size * s;
                            let (cx, cy) = ((r.left + r.right) / 2.0, (r.top + r.bottom) / 2.0);
                            let (src, dst) = img.fit(cx, cy, sz);
                            rt.DrawBitmap(&bmp, Some(&dst), 1.0, D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, Some(&src));
                        }
                    }
                    Hit::Monitor => {
                        if hovered {
                            rt.FillRoundedRectangle(&rr, &b_hover);
                        }
                        let dim = D2D1_COLOR_F { a: text.a * 0.6, ..text };
                        let Ok(b_dim) = rt.CreateSolidColorBrush(&dim, None) else { continue };
                        let draw = |t: &str, r: D2D_RECT_F, b: &ID2D1SolidColorBrush| {
                            let t: Vec<u16> = t.encode_utf16().collect();
                            rt.DrawText(&t, mon_fmt, &r, b, D2D1_DRAW_TEXT_OPTIONS_NONE, DWRITE_MEASURING_MODE_NATURAL);
                        };
                        if vertical {
                            // one line per metric: "CPU 12% 53°"
                            let top = (r.top + r.bottom - mon.len() as f32 * mon_line_h) / 2.0;
                            for (k, (label, value, hot)) in mon.iter().enumerate() {
                                let y = top + k as f32 * mon_line_h;
                                let b = if *hot { &b_attn } else { &b_text };
                                draw(&format!("{label} {value}"), rf(r.left, y, r.right, y + mon_line_h), b);
                            }
                        } else {
                            // a column per metric: dim label above its value
                            let widths: Vec<f32> = mon
                                .iter()
                                .map(|(l, v, _)| Self::measure(ctx.dw, mon_fmt, l).0.max(Self::measure(ctx.dw, mon_fmt, v).0))
                                .collect();
                            let total = widths.iter().sum::<f32>() + (mon.len() as f32 - 1.0) * 12.0 * s;
                            let mut x = (r.left + r.right - total) / 2.0;
                            let mid = (r.top + r.bottom) / 2.0;
                            for ((label, value, hot), w) in mon.iter().zip(&widths) {
                                draw(label, rf(x, mid - mon_line_h, x + w, mid), &b_dim);
                                let b = if *hot { &b_attn } else { &b_text };
                                draw(value, rf(x, mid, x + w, mid + mon_line_h), b);
                                x += w + 12.0 * s;
                            }
                        }
                    }
                    Hit::Lang => {
                        if hovered {
                            rt.FillRoundedRectangle(&rr, &b_hover);
                        }
                        let Some(l) = &lang else { continue };
                        let line_h = (r.bottom - r.top).min(c.font_size * 0.88 * 1.35 * s);
                        let top = (r.top + r.bottom - line_h) / 2.0;
                        let t: Vec<u16> = l.short.encode_utf16().collect();
                        rt.DrawText(&t, &small_fmt, &rf(r.left, top, r.right, top + line_h), &b_text, D2D1_DRAW_TEXT_OPTIONS_NONE, DWRITE_MEASURING_MODE_NATURAL);
                    }
                    Hit::Battery => {
                        if hovered {
                            rt.FillRoundedRectangle(&rr, &b_hover);
                        }
                        let low = battery.is_some_and(|b| b.percent <= 15 && !b.plugged_in);
                        let brush = if low { &b_attn } else { &b_text };
                        let g: Vec<u16> = bat_glyph.encode_utf16().collect();
                        let t: Vec<u16> = bat_text.encode_utf16().collect();
                        let (gr, tr) = if vertical {
                            let top = (r.top + r.bottom - bat_gh - bat_th) / 2.0;
                            (
                                rf(r.left, top, r.right, top + bat_gh),
                                rf(r.left, top + bat_gh, r.right, top + bat_gh + bat_th),
                            )
                        } else {
                            let left = (r.left + r.right - bat_gw - bat_gap - bat_tw) / 2.0;
                            (
                                rf(left, r.top, left + bat_gw, r.bottom),
                                rf(left + bat_gw + bat_gap, r.top, left + bat_gw + bat_gap + bat_tw, r.bottom),
                            )
                        };
                        rt.DrawText(&g, &glyph_fmt, &gr, brush, D2D1_DRAW_TEXT_OPTIONS_NONE, DWRITE_MEASURING_MODE_NATURAL);
                        rt.DrawText(&t, &small_fmt, &tr, brush, D2D1_DRAW_TEXT_OPTIONS_NONE, DWRITE_MEASURING_MODE_NATURAL);
                    }
                    Hit::Overflow => {
                        if hovered || ctx.overflow_open {
                            rt.FillRoundedRectangle(&rr, &b_hover);
                        }
                        // chevron pointing where the flyout opens (Segoe Fluent Icons)
                        let glyph = match c.position {
                            Position::Bottom => "\u{E70E}",
                            Position::Top => "\u{E70D}",
                            Position::Left => "\u{E76C}",
                            Position::Right => "\u{E76B}",
                        };
                        let t: Vec<u16> = glyph.encode_utf16().collect();
                        rt.DrawText(&t, &glyph_fmt, r, &b_text, D2D1_DRAW_TEXT_OPTIONS_NONE, DWRITE_MEASURING_MODE_NATURAL);
                    }
                    Hit::Quick => {
                        if hovered {
                            rt.FillRoundedRectangle(&rr, &b_hover);
                        }
                        let t: Vec<u16> = quick_text.encode_utf16().collect();
                        rt.DrawText(&t, &glyph_fmt, r, &b_text, D2D1_DRAW_TEXT_OPTIONS_NONE, DWRITE_MEASURING_MODE_NATURAL);
                    }
                    Hit::Clock => {
                        if hovered {
                            rt.FillRoundedRectangle(&rr, &b_hover);
                        }
                        let t: Vec<u16> = ctx.time.encode_utf16().collect();
                        if two_lines {
                            let mid = (r.top + r.bottom) / 2.0;
                            let line = c.font_size * 1.35 * s;
                            let tr = rf(r.left, mid - line, r.right, mid);
                            let dr = rf(r.left, mid, r.right, mid + line);
                            rt.DrawText(&t, &text_fmt, &tr, &b_text, D2D1_DRAW_TEXT_OPTIONS_NONE, DWRITE_MEASURING_MODE_NATURAL);
                            let d: Vec<u16> = ctx.date.encode_utf16().collect();
                            rt.DrawText(&d, &small_fmt, &dr, &b_text, D2D1_DRAW_TEXT_OPTIONS_NONE, DWRITE_MEASURING_MODE_NATURAL);
                        } else {
                            rt.DrawText(&t, &text_fmt, r, &b_text, D2D1_DRAW_TEXT_OPTIONS_NONE, DWRITE_MEASURING_MODE_NATURAL);
                        }
                    }
                }
            }

            if rt.EndDraw(None, None).is_err() {
                // device lost (D2DERR_RECREATE_TARGET) or similar: rebuild next time
                self.reset_target();
            }
        }
        // free bitmaps of icons that are gone
        self.bitmaps.retain(|id, _| used.contains(id));
        self.update_tooltips(ctx);
    }

    fn tip_text(hit: Hit, ctx: &PaintCtx) -> String {
        match hit {
            Hit::Start => "Start".into(),
            Hit::Desktop => "Show desktop".into(),
            Hit::Quick => "Quick settings".into(),
            Hit::Overflow => "Show hidden icons".into(),
            Hit::Monitor => ctx.stats.map(|st| st.tooltip()).unwrap_or_default(),
            Hit::Battery => ctx.battery.map(|b| b.tip()).unwrap_or_default(),
            Hit::Lang => ctx.lang.map(|l| l.tooltip()).unwrap_or_default(),
            Hit::Clock => ctx.long_date.to_string(),
            Hit::Tray(i) => ctx.tray.icons.get(i).map(|t| t.tip.clone()).unwrap_or_default(),
            Hit::App(i) => match ctx.groups.get(i) {
                Some(g) if g.windows.len() == 1 => g.windows[0].title.clone(),
                Some(g) if g.windows.len() > 1 => {
                    let mut s = format!("{} — {} windows", g.name, g.windows.len());
                    for w in g.windows.iter().take(8) {
                        s.push_str("\n• ");
                        s.push_str(&w.title);
                    }
                    s
                }
                Some(g) => g.name.clone(),
                None => String::new(),
            },
        }
    }

    fn update_tooltips(&mut self, ctx: &PaintCtx) {
        if self.tooltip.is_invalid() {
            return;
        }
        let tips: Vec<(D2D_RECT_F, String)> = self
            .items
            .iter()
            .map(|(h, r)| (*r, Self::tip_text(*h, ctx)))
            .collect();
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for (r, t) in &tips {
            (r.left as i32, r.top as i32, r.right as i32, r.bottom as i32).hash(&mut hasher);
            t.hash(&mut hasher);
        }
        let sig = hasher.finish();
        if sig == self.tips_sig {
            return;
        }
        self.tips_sig = sig;
        unsafe {
            for id in 0..self.tips_count {
                let ti = TTTOOLINFOW {
                    cbSize: std::mem::size_of::<TTTOOLINFOW>() as u32,
                    hwnd: self.hwnd,
                    uId: id,
                    ..Default::default()
                };
                SendMessageW(self.tooltip, TTM_DELTOOLW, None, Some(LPARAM(&ti as *const _ as isize)));
            }
            for (id, (r, t)) in tips.iter().enumerate() {
                if t.is_empty() {
                    continue;
                }
                let mut text = util::wide(t);
                let ti = TTTOOLINFOW {
                    cbSize: std::mem::size_of::<TTTOOLINFOW>() as u32,
                    uFlags: TTF_SUBCLASS,
                    hwnd: self.hwnd,
                    uId: id,
                    rect: RECT {
                        left: r.left as i32,
                        top: r.top as i32,
                        right: r.right as i32,
                        bottom: r.bottom as i32,
                    },
                    lpszText: PWSTR(text.as_mut_ptr()),
                    ..Default::default()
                };
                SendMessageW(self.tooltip, TTM_ADDTOOLW, None, Some(LPARAM(&ti as *const _ as isize)));
            }
        }
        self.tips_count = tips.len();
    }
}
