//! Tray overflow flyout: the grid of hidden tray icons opened by the bar's ^ button.
//! Closes when it loses focus, like Windows' own overflow window.

use std::{
    collections::{HashMap, HashSet},
    time::Instant,
};

use windows::Win32::{
    Foundation::{HWND, RECT},
    Graphics::{
        Direct2D::{
            Common::{D2D_RECT_F, D2D_SIZE_U},
            D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, D2D1_ROUNDED_RECT, ID2D1Bitmap, ID2D1Factory,
            ID2D1HwndRenderTarget,
        },
        Gdi::InvalidateRect,
    },
    UI::WindowsAndMessaging::{
        CreateWindowExW, HWND_TOPMOST, SW_HIDE, SWP_SHOWWINDOW, SetForegroundWindow, SetWindowPos,
        ShowWindow, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
    },
};
use windows::core::w;

use crate::{
    bar::{cached_bitmap, ensure_target, rf},
    config::{Config, Position},
    tray::Tray,
    util::parse_color,
};

/// Where the flyout was opened from, kept so it can re-layout when icons change.
#[derive(Clone, Copy)]
struct Anchor {
    button: RECT,
    bar: RECT,
    monitor: RECT,
    scale: f32,
    pos: Position,
}

pub struct Flyout {
    pub hwnd: HWND,
    rt: Option<ID2D1HwndRenderTarget>,
    bitmaps: HashMap<u64, ID2D1Bitmap>,
    /// (tray icon index, rect in client px)
    items: Vec<(usize, D2D_RECT_F)>,
    pub hover: Option<usize>,
    pub pressed: Option<usize>,
    pub tracking: bool,
    pub swallow_up: bool,
    pub open: bool,
    pub closed_at: Instant,
    anchor: Option<Anchor>,
    size: (i32, i32),
    /// top-left corner on screen
    origin: (i32, i32),
    /// Set after a click forwarded to an icon: losing focus to that icon's app
    /// (its menu or popup) must not close the flyout. Holds the app's process id.
    pub interacting_pid: Option<u32>,
}

impl Flyout {
    pub fn new(hinst: windows::Win32::Foundation::HINSTANCE) -> Option<Flyout> {
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
                w!("DocksyFlyout"),
                w!("Docksy tray overflow"),
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
        Some(Flyout {
            hwnd,
            rt: None,
            bitmaps: HashMap::new(),
            items: Vec::new(),
            hover: None,
            pressed: None,
            tracking: false,
            swallow_up: false,
            open: false,
            closed_at: Instant::now(),
            anchor: None,
            size: (1, 1),
            origin: (0, 0),
            interacting_pid: None,
        })
    }

    pub fn hit_test(&self, x: i32, y: i32) -> Option<usize> {
        let (x, y) = (x as f32, y as f32);
        self.items
            .iter()
            .find(|(_, r)| x >= r.left && x < r.right && y >= r.top && y < r.bottom)
            .map(|(i, _)| *i)
    }

    /// Opens next to the ^ button. `button`, `bar` and `monitor` are screen rects.
    pub fn show(
        &mut self,
        cfg: &Config,
        tray: &Tray,
        button: RECT,
        bar: RECT,
        monitor: RECT,
        scale: f32,
    ) {
        self.anchor = Some(Anchor {
            button,
            bar,
            monitor,
            scale,
            pos: cfg.position,
        });
        crate::backdrop::apply(self.hwnd, cfg, true);
        self.open = true;
        self.relayout(cfg, tray);
        unsafe {
            let _ = SetForegroundWindow(self.hwnd);
        }
    }

    pub fn hide(&mut self) {
        if !self.open {
            return;
        }
        self.open = false;
        self.closed_at = Instant::now();
        self.interacting_pid = None;
        self.hover = None;
        self.pressed = None;
        self.tracking = false;
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
    }

    /// Recomputes size, position and the icon grid (e.g. after tray icons changed).
    pub fn relayout(&mut self, cfg: &Config, tray: &Tray) {
        if !self.open {
            return;
        }
        let Some(a) = self.anchor else { return };
        let icons = tray.overflow(cfg.tray_show_all);
        if icons.is_empty() {
            self.hide();
            return;
        }
        let s = a.scale;
        let n = icons.len();
        let cols = n.clamp(1, 5);
        let rows = n.div_ceil(cols);
        let cell = (cfg.tray_icon_size + 18.0) * s;
        let pad = 8.0 * s;
        let w = (cols as f32 * cell + 2.0 * pad).round() as i32;
        let h = (rows as f32 * cell + 2.0 * pad).round() as i32;
        let gap = (8.0 * s).round() as i32;
        let (cx, cy) = ((a.button.left + a.button.right) / 2, (a.button.top + a.button.bottom) / 2);
        let (mut x, mut y) = match a.pos {
            Position::Bottom => (cx - w / 2, a.bar.top - gap - h),
            Position::Top => (cx - w / 2, a.bar.bottom + gap),
            Position::Left => (a.bar.right + gap, cy - h / 2),
            Position::Right => (a.bar.left - gap - w, cy - h / 2),
        };
        x = x.clamp(a.monitor.left + gap, (a.monitor.right - gap - w).max(a.monitor.left));
        y = y.clamp(a.monitor.top + gap, (a.monitor.bottom - gap - h).max(a.monitor.top));

        self.items = icons
            .iter()
            .enumerate()
            .map(|(k, &idx)| {
                let (col, row) = ((k % cols) as f32, (k / cols) as f32);
                let l = pad + col * cell;
                let t = pad + row * cell;
                (idx, rf(l, t, l + cell, t + cell))
            })
            .collect();
        self.size = (w, h);
        self.origin = (x, y);
        unsafe {
            let _ = SetWindowPos(self.hwnd, Some(HWND_TOPMOST), x, y, w, h, SWP_SHOWWINDOW);
            let _ = InvalidateRect(Some(self.hwnd), None, false);
        }
    }

    /// Screen rectangle of tray icon `idx`, if it is in the open flyout.
    pub fn icon_screen_rect(&self, idx: usize) -> Option<RECT> {
        if !self.open {
            return None;
        }
        let (_, r) = self.items.iter().find(|(i, _)| *i == idx)?;
        Some(RECT {
            left: self.origin.0 + r.left as i32,
            top: self.origin.1 + r.top as i32,
            right: self.origin.0 + r.right as i32,
            bottom: self.origin.1 + r.bottom as i32,
        })
    }

    pub fn reset_target(&mut self) {
        self.rt = None;
        self.bitmaps.clear();
    }

    pub fn paint(&mut self, d2d: &ID2D1Factory, cfg: &Config, tray: &Tray) {
        if !self.open {
            return;
        }
        let size = D2D_SIZE_U {
            width: self.size.0.max(1) as u32,
            height: self.size.1.max(1) as u32,
        };
        let Some(rt) = ensure_target(&mut self.rt, d2d, self.hwnd, size) else { return };
        let s = self.anchor.map(|a| a.scale).unwrap_or(1.0);
        let _bg = parse_color(&cfg.background);
        let hover = parse_color(&cfg.hover);
        let Ok(b_hover) = (unsafe { rt.CreateSolidColorBrush(&hover, None) }) else { return };
        let mut used = HashSet::new();
        let items = self.items.clone();
        unsafe {
            rt.BeginDraw();
            rt.Clear(Some(&crate::backdrop::clear_color(cfg)));
            for (idx, r) in &items {
                if self.hover == Some(*idx) {
                    let rr = D2D1_ROUNDED_RECT { rect: *r, radiusX: 6.0 * s, radiusY: 6.0 * s };
                    rt.FillRoundedRectangle(&rr, &b_hover);
                }
                let Some(img) = tray.icons.get(*idx).and_then(|t| t.image.clone()) else { continue };
                if let Some(bmp) = cached_bitmap(&mut self.bitmaps, &rt, &img) {
                    used.insert(img.id);
                    let sz = cfg.tray_icon_size * s;
                    let (cx, cy) = ((r.left + r.right) / 2.0, (r.top + r.bottom) / 2.0);
                    let (src, dst) = img.fit(cx, cy, sz);
                    rt.DrawBitmap(&bmp, Some(&dst), 1.0, D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, Some(&src));
                }
            }
            if rt.EndDraw(None, None).is_err() {
                self.reset_target();
            }
        }
        self.bitmaps.retain(|id, _| used.contains(id));
    }
}
