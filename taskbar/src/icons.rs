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
        Some(Image {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            w,
            h,
            px,
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

    pub fn clear(&mut self) {
        self.map.clear();
    }
}
