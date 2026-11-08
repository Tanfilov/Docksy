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
}

fn rf(l: f32, t: f32, r: f32, b: f32) -> D2D_RECT_F {
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
            Some(f)
        }
    }

    fn measure(dw: &IDWriteFactory, fmt: &IDWriteTextFormat, text: &str) -> f32 {
        let t: Vec<u16> = text.encode_utf16().collect();
        unsafe {
            let Ok(layout) = dw.CreateTextLayout(&t, fmt, 10000.0, 1000.0) else {
                return 0.0;
            };
            let mut m = DWRITE_TEXT_METRICS::default();
            if layout.GetMetrics(&mut m).is_err() {
                return 0.0;
            }
            m.widthIncludingTrailingWhitespace
        }
    }

    /// Computes item rects (physical px, client coordinates).
    fn layout(&self, ctx: &PaintCtx, clock_w: f32, quick_w: f32, tray: &[usize]) -> Vec<(Hit, D2D_RECT_F)> {
        let c = ctx.cfg;
        let s = self.scale;
        let (w, h) = (self.width(), self.height());
        let pad = 4.0 * s;
        let (top, bottom) = (pad, h - pad);
        let mut items = Vec::new();

        // right side, from the right edge inwards
        let mut x = w;
        if c.show_desktop_button {
            let dw = 10.0 * s;
            items.push((Hit::Desktop, rf(x - dw, 0.0, x, h)));
            x -= dw;
        }
        x -= 4.0 * s;
        let show_tray = self.primary || c.tray_on_all_monitors;
        if show_tray || self.primary {
            let cw = clock_w + 20.0 * s;
            items.push((Hit::Clock, rf(x - cw, top, x, bottom)));
            x -= cw;
        }
        if c.show_quick_settings && show_tray {
            let qw = quick_w + 20.0 * s;
            items.push((Hit::Quick, rf(x - qw, top, x, bottom)));
            x -= qw;
        }
        if show_tray {
            let tw = (c.tray_icon_size + 14.0) * s;
            for &i in tray.iter().rev() {
                items.push((Hit::Tray(i), rf(x - tw, top, x, bottom)));
                x -= tw;
            }
        }
        let right_limit = x - 8.0 * s;

        // app strip: [start][apps...]
        let mut strip: Vec<Hit> = Vec::new();
        if c.show_start_button {
            strip.push(Hit::Start);
        }
        strip.extend((0..ctx.groups.len()).map(Hit::App));
        let gap = c.item_spacing * s;
        let mut cell = (c.icon_size + 20.0).max(c.icon_size * 1.4) * s;
        let n = strip.len() as f32;
        let avail = (right_limit - 8.0 * s).max(0.0);
        let mut strip_w = n * cell + (n - 1.0).max(0.0) * gap;
        if strip_w > avail && n > 0.0 {
            cell = ((avail - (n - 1.0) * gap) / n).max(8.0 * s);
            strip_w = n * cell + (n - 1.0).max(0.0) * gap;
        }
        let mut x0 = if c.center_apps { (w - strip_w) / 2.0 } else { 8.0 * s };
        x0 = x0.min(right_limit - strip_w).max(8.0 * s);
        for hit in strip {
            items.push((hit, rf(x0, top, x0 + cell, bottom)));
            x0 += cell + gap;
        }
        items
    }

    fn bitmap(&mut self, rt: &ID2D1HwndRenderTarget, img: &Image) -> Option<ID2D1Bitmap> {
        if let Some(b) = self.bitmaps.get(&img.id) {
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
        self.bitmaps.insert(img.id, b.clone());
        Some(b)
    }

    fn ensure_target(&mut self, d2d: &ID2D1Factory) -> Option<ID2D1HwndRenderTarget> {
        let size = D2D_SIZE_U {
            width: self.width().max(1.0) as u32,
            height: self.height().max(1.0) as u32,
        };
        if let Some(rt) = &self.rt {
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
            hwnd: self.hwnd,
            pixelSize: size,
            presentOptions: D2D1_PRESENT_OPTIONS_NONE,
        };
        let rt = unsafe { d2d.CreateHwndRenderTarget(&props, &hprops) }.ok()?;
        self.rt = Some(rt.clone());
        Some(rt)
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

        let two_lines = c.show_date && c.height >= 40.0 && !ctx.date.is_empty();
        let clock_w = Self::measure(ctx.dw, &text_fmt, ctx.time)
            .max(if two_lines { Self::measure(ctx.dw, &small_fmt, ctx.date) } else { 0.0 });
        let quick_w = Self::measure(ctx.dw, &glyph_fmt, ctx.quick);
        let tray_idx: Vec<usize> = ctx.tray.visible().map(|(i, _)| i).collect();
        self.items = self.layout(ctx, clock_w, quick_w, &tray_idx);

        let bg = parse_color(&c.background);
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
            rt.Clear(Some(&bg));

            for (hit, r) in &items {
                let rr = D2D1_ROUNDED_RECT { rect: *r, radiusX: radius, radiusY: radius };
                let hovered = self.hover == Some(*hit);
                match hit {
                    Hit::Desktop => {
                        // thin separator line
                        let x = r.left + 0.5;
                        rt.FillRectangle(&rf(x, r.top + 12.0 * s, x + 1.0 * s, r.bottom - 12.0 * s), &b_hover);
                        if hovered {
                            rt.FillRectangle(r, &b_hover);
                        }
                    }
                    Hit::Start => {
                        if hovered {
                            rt.FillRoundedRectangle(&rr, &b_hover);
                        }
                        // four squares
                        let size = c.icon_size * 0.62 * s;
                        let (cx, cy) = ((r.left + r.right) / 2.0, (r.top + r.bottom) / 2.0);
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
                        if let Some(img) = ctx.icons.get(&grp.icon_sources, px, first) {
                            if let Some(bmp) = self.bitmap(&rt, &img) {
                                used.insert(img.id);
                                let (cx, cy) = ((r.left + r.right) / 2.0, (r.top + r.bottom) / 2.0 - 1.0 * s);
                                let half = px as f32 / 2.0;
                                let dst = rf((cx - half).round(), (cy - half).round(), (cx - half).round() + px as f32, (cy - half).round() + px as f32);
                                rt.DrawBitmap(&bmp, Some(&dst), 1.0, D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, None);
                            }
                        }
                        if !grp.windows.is_empty() {
                            let iw = if focused { 18.0 * s } else { 7.0 * s };
                            let ih = 3.0 * s;
                            let cx = (r.left + r.right) / 2.0;
                            let y = if c.position == Position::Top { r.top + 1.0 * s } else { r.bottom - ih - 1.0 * s };
                            let ind = D2D1_ROUNDED_RECT { rect: rf(cx - iw / 2.0, y, cx + iw / 2.0, y + ih), radiusX: ih / 2.0, radiusY: ih / 2.0 };
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
                            let dst = rf((cx - sz / 2.0).round(), (cy - sz / 2.0).round(), (cx - sz / 2.0).round() + sz, (cy - sz / 2.0).round() + sz);
                            rt.DrawBitmap(&bmp, Some(&dst), 1.0, D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, None);
                        }
                    }
                    Hit::Quick => {
                        if hovered {
                            rt.FillRoundedRectangle(&rr, &b_hover);
                        }
                        let t: Vec<u16> = ctx.quick.encode_utf16().collect();
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
