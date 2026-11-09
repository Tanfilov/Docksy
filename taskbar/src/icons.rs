//! Icon loading. Every icon is converted once into a premultiplied BGRA pixel buffer
//! (`Image`), which each bar uploads to its own Direct2D render target.

use std::{
    cell::RefCell,
    collections::HashMap,
    rc::Rc,
    sync::atomic::{AtomicU64, Ordering},
};

use windows::Win32::{
    Foundation::{HWND, LPARAM, SIZE, WPARAM},
    Graphics::{
        Gdi::{DeleteObject, HBITMAP, HGDIOBJ},
        Imaging::{
            CLSID_WICImagingFactory, GUID_WICPixelFormat32bppPBGRA, IWICBitmapSource,
            IWICImagingFactory, WICBitmapDitherTypeNone, WICBitmapPaletteTypeCustom,
            WICBitmapUsePremultipliedAlpha,
        },
    },
    System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance},
    UI::{
        Shell::{
            IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_BIGGERSIZEOK,
            SIIGBF_ICONONLY,
        },
        WindowsAndMessaging::{
            GCLP_HICON, GetClassLongPtrW, HICON, ICON_BIG, SMTO_ABORTIFHUNG, SMTO_BLOCK,
            SendMessageTimeoutW,
            WM_GETICON,
        },
    },
};
use windows::core::{HSTRING, Interface};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

pub struct Image {
    pub id: u64,
    pub w: u32,
    pub h: u32,
    /// premultiplied BGRA, stride = w * 4
    pub px: Vec<u8>,
    /// visible artwork (left, top, right, bottom) without transparent margins
    pub content: (u32, u32, u32, u32),
}

/// Bounding box of the pixels that are clearly visible (alpha > 24), so icons with
/// different transparent margins can be drawn at the same visual size.
fn visible_bounds(px: &[u8], w: u32, h: u32) -> (u32, u32, u32, u32) {
    let (mut l, mut t, mut r, mut b) = (w, h, 0u32, 0u32);
    for y in 0..h {
        for x in 0..w {
            if px[((y * w + x) * 4 + 3) as usize] > 24 {
                l = l.min(x);
                t = t.min(y);
                r = r.max(x + 1);
                b = b.max(y + 1);
            }
        }
    }
    if r <= l || b <= t { (0, 0, w, h) } else { (l, t, r, b) }
}

impl Image {
    /// Where to draw this icon so its visible artwork fills a `size` square
    /// centered at (cx, cy), keeping its aspect ratio: returns (source, dest) rects.
    pub fn fit(
        &self,
        cx: f32,
        cy: f32,
        size: f32,
    ) -> (
        windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F,
        windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F,
    ) {
        use windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F;
        let (l, t, r, b) = self.content;
        let (cw, ch) = ((r - l) as f32, (b - t) as f32);
        let k = size / cw.max(ch);
        let (dw, dh) = (cw * k, ch * k);
        let (dl, dt) = ((cx - dw / 2.0).round(), (cy - dh / 2.0).round());
        (
            D2D_RECT_F { left: l as f32, top: t as f32, right: r as f32, bottom: b as f32 },
            D2D_RECT_F { left: dl, top: dt, right: dl + dw.round(), bottom: dt + dh.round() },
        )
    }
}

thread_local! {
    static WIC: RefCell<Option<IWICImagingFactory>> = const { RefCell::new(None) };
}

fn wic() -> Option<IWICImagingFactory> {
    WIC.with(|w| {
        let mut w = w.borrow_mut();
        if w.is_none() {
            *w = unsafe { CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER) }.ok();
        }
        w.clone()
    })
}

fn from_source(src: &IWICBitmapSource) -> Option<Image> {
    let f = wic()?;
    unsafe {
        let conv = f.CreateFormatConverter().ok()?;
        conv.Initialize(
            src,
            &GUID_WICPixelFormat32bppPBGRA,
            WICBitmapDitherTypeNone,
            None,
            0.0,
            WICBitmapPaletteTypeCustom,
        )
        .ok()?;
        let (mut w, mut h) = (0u32, 0u32);
        conv.GetSize(&mut w, &mut h).ok()?;
        if w == 0 || h == 0 || w > 1024 || h > 1024 {
            return None;
        }
        let mut px = vec![0u8; (w * h * 4) as usize];
        conv.CopyPixels(std::ptr::null(), w * 4, &mut px).ok()?;
        // fully transparent images are useless
        if px.chunks_exact(4).all(|p| p[3] == 0) {
            return None;
        }
        let content = visible_bounds(&px, w, h);
        Some(Image {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            w,
            h,
            px,
            content,
        })
    }
}

pub fn from_hicon(hicon: HICON) -> Option<Image> {
    if hicon.is_invalid() {
        return None;
    }
    let f = wic()?;
    let bmp = unsafe { f.CreateBitmapFromHICON(hicon) }.ok()?;
    from_source(&bmp.cast().ok()?)
}

fn from_hbitmap(hbm: HBITMAP) -> Option<Image> {
    let f = wic()?;
    let bmp = unsafe { f.CreateBitmapFromHBITMAP(hbm, windows::Win32::Graphics::Gdi::HPALETTE::default(), WICBitmapUsePremultipliedAlpha) }.ok()?;
    from_source(&bmp.cast().ok()?)
}

/// Icon for a file path or a shell parsing name like `shell:AppsFolder\<AUMID>`.
pub fn from_shell(name: &str, size: u32) -> Option<Image> {
    unsafe {
        let factory: IShellItemImageFactory =
            SHCreateItemFromParsingName(&HSTRING::from(name), None).ok()?;
        let hbm = factory
            .GetImage(
                SIZE {
                    cx: size as i32,
                    cy: size as i32,
                },
                SIIGBF_ICONONLY | SIIGBF_BIGGERSIZEOK,
            )
            .ok()?;
        let img = from_hbitmap(hbm);
        let _ = DeleteObject(HGDIOBJ(hbm.0));
        img
    }
}

/// Icon the window itself reports (fallback for apps whose exe has no icon).
pub fn from_window(hwnd: HWND) -> Option<Image> {
    unsafe {
        let mut res = 0usize;
        SendMessageTimeoutW(
            hwnd,
            WM_GETICON,
            WPARAM(ICON_BIG as usize),
            LPARAM(0),
            SMTO_ABORTIFHUNG | SMTO_BLOCK,
            50,
            Some(&mut res),
        );
        if res == 0 {
            res = GetClassLongPtrW(hwnd, GCLP_HICON);
        }
        from_hicon(HICON(res as *mut _))
    }
}

/// Cache of app icons, keyed by source and pixel size.
#[derive(Default)]
pub struct IconCache {
    map: HashMap<(String, u32), Option<Rc<Image>>>,
}

impl IconCache {
    pub fn get(&mut self, sources: &[String], size: u32, hwnd: Option<HWND>) -> Option<Rc<Image>> {
        let key = (sources.first().cloned().unwrap_or_default(), size);
        if let Some(v) = self.map.get(&key) {
            return v.clone();
        }
        let img = sources
            .iter()
            .filter(|s| !s.is_empty())
            .find_map(|s| from_shell(s, size))
            .or_else(|| hwnd.and_then(from_window))
            .map(Rc::new);
        self.map.insert(key, img.clone());
        img
    }

    /// Any already-loaded size of this icon (largest first), without loading.
    /// Used while a size slider is dragged: drawing scales it, and the exact size
    /// is loaded once the slider is released.
    pub fn nearest(&self, sources: &[String]) -> Option<Rc<Image>> {
        let first = sources.first()?;
        self.map
            .iter()
            .filter(|((src, _), img)| src == first && img.is_some())
            .max_by_key(|((_, size), _)| *size)
            .and_then(|(_, img)| img.clone())
    }

    pub fn clear(&mut self) {
        self.map.clear();
    }

    /// A cached result for one source (`None` = not loaded yet; `Some(None)` = it
    /// has no icon). Never loads.
    pub fn cached(&self, source: &str, size: u32) -> Option<Option<Rc<Image>>> {
        self.map.get(&(source.to_string(), size)).cloned()
    }

    pub fn insert(&mut self, source: String, size: u32, img: Option<Image>) {
        self.map.insert((source, size), img.map(Rc::new));
    }
}

// ---------------------------------------------------------------------------
// background loading (Start panel): shell icons can take tens of milliseconds
// each, which made scrolling stutter when they loaded during painting
// ---------------------------------------------------------------------------

type Loaded = ((String, u32), Option<Image>);
static LOADED: std::sync::Mutex<Vec<Loaded>> = std::sync::Mutex::new(Vec::new());

pub struct IconLoader {
    tx: std::sync::mpsc::Sender<(String, u32)>,
    pending: std::collections::HashSet<(String, u32)>,
}

impl IconLoader {
    /// `notify` gets `msg` when loaded icons are waiting in `take_loaded`.
    pub fn new(notify: HWND, msg: u32) -> IconLoader {
        let (tx, rx) = std::sync::mpsc::channel::<(String, u32)>();
        let target = notify.0 as isize;
        std::thread::spawn(move || {
            unsafe {
                let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED);
            }
            for (src, size) in rx {
                let img = from_shell(&src, size);
                let first = match LOADED.lock() {
                    Ok(mut v) => {
                        v.push(((src, size), img));
                        v.len() == 1
                    }
                    Err(_) => false,
                };
                // one message per batch: the UI takes everything loaded so far
                if first {
                    unsafe {
                        let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                            Some(HWND(target as *mut _)),
                            msg,
                            WPARAM(0),
                            LPARAM(0),
                        );
                    }
                }
            }
        });
        IconLoader { tx, pending: Default::default() }
    }

    /// Queues an icon (once); requests are served in order.
    pub fn request(&mut self, source: &str, size: u32) {
        let key = (source.to_string(), size);
        if self.pending.insert(key.clone()) {
            let _ = self.tx.send(key);
        }
    }

    /// Moves finished icons into `cache`. Returns whether there were any.
    pub fn take_loaded(&mut self, cache: &mut IconCache) -> bool {
        let done: Vec<Loaded> = LOADED.lock().map(|mut v| std::mem::take(&mut *v)).unwrap_or_default();
        let any = !done.is_empty();
        for (key, img) in done {
            self.pending.remove(&key);
            cache.insert(key.0, key.1, img);
        }
        any
    }
}
