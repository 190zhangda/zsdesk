//! Shell icons and thumbnails.
//!
//! Tab, bookmark and address icons come from the system image list (same
//! icons Explorer shows). Desktop fences request icons/thumbnails from a
//! background STA thread through `IShellItemImageFactory`; results come back
//! to a window as posted `HBITMAP`s.

use std::{collections::{HashSet, VecDeque}, sync::{Arc, Condvar, Mutex}, thread};

use windows::{
    core::{Interface, PCWSTR},
    Win32::{
        Foundation::{HWND, LPARAM, SIZE, WPARAM},
        Graphics::Gdi::{DeleteObject, GetObjectW, DIBSECTION, HBITMAP},
        Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES,
        System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE},
        UI::{
            Controls::{IImageList, ILD_TRANSPARENT},
            Shell::{
                IShellItemImageFactory, SHGetFileInfoW, SHGetImageList, SHFILEINFOW, SHGFI_PIDL, SHGFI_SYSICONINDEX,
                SHIL_EXTRALARGE, SHIL_JUMBO, SHIL_LARGE, SHIL_SMALL, SIIGBF_BIGGERSIZEOK, SIIGBF_ICONONLY, SIIGBF_RESIZETOFIT,
                SHGFI_USEFILEATTRIBUTES,
            },
            WindowsAndMessaging::{DestroyIcon, PostMessageW, HICON},
        },
    },
};

use crate::gfx::BitmapAlpha;

/// System image list index for a PIDL.
pub fn icon_index(pidl: &[u8]) -> Option<i32> {
    let mut info = SHFILEINFOW::default();
    let ok = unsafe { SHGetFileInfoW(PCWSTR(pidl.as_ptr().cast()), FILE_FLAGS_AND_ATTRIBUTES(0), Some(&mut info),
        std::mem::size_of::<SHFILEINFOW>() as u32, SHGFI_PIDL | SHGFI_SYSICONINDEX) };
    (ok != 0).then_some(info.iIcon)
}

/// Icon index for a file name pattern without touching the disk.
pub fn icon_index_for_name(name: &str, directory: bool) -> Option<i32> {
    let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    let attrs = if directory { windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY } else { windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL };
    let mut info = SHFILEINFOW::default();
    let ok = unsafe { SHGetFileInfoW(PCWSTR(wide.as_ptr()), attrs, Some(&mut info),
        std::mem::size_of::<SHFILEINFOW>() as u32, SHGFI_SYSICONINDEX | SHGFI_USEFILEATTRIBUTES) };
    (ok != 0).then_some(info.iIcon)
}

/// Extracts an icon of at least `pixels` size from the system image list. Caller owns the HICON.
pub fn system_icon(index: i32, pixels: i32) -> Option<HICON> {
    let list = if pixels <= 16 { SHIL_SMALL } else if pixels <= 32 { SHIL_LARGE } else if pixels <= 48 { SHIL_EXTRALARGE } else { SHIL_JUMBO };
    let images: IImageList = unsafe { SHGetImageList(list as i32).ok()? };
    unsafe { images.GetIcon(index, ILD_TRANSPARENT.0).ok() }
}

/// Owned icon handle.
pub struct OwnedIcon(pub HICON);
impl Drop for OwnedIcon { fn drop(&mut self) { let _ = unsafe { DestroyIcon(self.0) }; } }

pub fn pidl_icon(pidl: &[u8], pixels: i32) -> Option<OwnedIcon> {
    system_icon(icon_index(pidl)?, pixels).map(OwnedIcon)
}

/// A finished icon or thumbnail.
pub struct IconResult {
    pub key: String,
    pub size: u32,
    pub token: u64,
    pub bitmap: HBITMAP,
    pub alpha: BitmapAlpha,
    pub pixel_bytes: u64,
}

impl IconResult {
    /// Releases the bitmap if the result is discarded.
    pub fn discard(self) {}
}
impl Drop for IconResult {
    fn drop(&mut self) { if !self.bitmap.is_invalid() { let _ = unsafe { DeleteObject(self.bitmap.into()) }; } }
}
// The bitmap has one owner and is never selected into a DC on the loader thread.
// GDI bitmap handles may be transferred within a process; the receiver either
// materializes its WIC copy or drops this result before anyone else uses it.
unsafe impl Send for IconResult {}

pub struct IconRequest { pub key: String, pub pidl: Vec<u8>, pub size: u32, pub thumbnail: bool, pub token: u64 }

const MAX_PENDING: usize = 128;
const MAX_READY: usize = 16;
const MAX_READY_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Default)]
struct LoaderState {
    requests: VecDeque<IconRequest>,
    pending: HashSet<u64>,
    ready: VecDeque<IconResult>,
    ready_bytes: u64,
    stopping: bool,
}
impl LoaderState {
    fn enqueue(&mut self, request: IconRequest) -> bool {
        if self.stopping || self.pending.len() >= MAX_PENDING || !self.pending.insert(request.token) { return false; }
        self.requests.push_back(request);
        true
    }
    fn retain(&mut self, tokens: &HashSet<u64>) {
        self.pending.retain(|t| tokens.contains(t));
        self.requests.retain(|r| tokens.contains(&r.token));
        self.ready.retain(|r| tokens.contains(&r.token));
        self.ready_bytes = self.ready.iter().map(|r| r.pixel_bytes).sum();
    }
    fn stop(&mut self) {
        self.stopping = true;
        self.requests.clear(); self.pending.clear(); self.ready.clear(); self.ready_bytes = 0;
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct IconLoaderStats { pub pending: usize, pub queued: usize, pub ready: usize, pub ready_pixel_bytes: u64 }

/// A bounded, cancellable queue. Window messages only wake the owner; this
/// object owns every completed bitmap, including results not consumed at exit.
pub struct IconLoader { shared: Arc<(Mutex<LoaderState>, Condvar)>, thread: Option<thread::JoinHandle<()>> }

impl IconLoader {
    /// `message` has no payload. Call `take_ready` on the receiving thread.
    pub fn spawn(hwnd: HWND, message: u32) -> Self {
        let shared = Arc::new((Mutex::new(LoaderState::default()), Condvar::new()));
        let worker = shared.clone();
        let target = hwnd.0 as isize;
        let thread = thread::Builder::new().name("zsdesk-icons".into()).spawn(move || {
            let initialized = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) }.is_ok();
            let (lock, wake) = &*worker;
            loop {
                let request = {
                    let mut state = lock.lock().unwrap_or_else(|p| p.into_inner());
                    while !state.stopping && state.requests.is_empty() { state = wake.wait(state).unwrap_or_else(|p| p.into_inner()); }
                    if state.stopping { break; }
                    state.requests.pop_front().unwrap()
                };
                let loaded = initialized.then(|| load(&request)).flatten();
                let (bitmap, alpha) = loaded.unwrap_or((HBITMAP::default(), BitmapAlpha::Ignore));
                let result = IconResult { key: request.key, size: request.size, token: request.token,
                    bitmap, alpha, pixel_bytes: bitmap_bytes(bitmap) };
                let should_post = {
                    let mut state = lock.lock().unwrap_or_else(|p| p.into_inner());
                    while !state.stopping && state.pending.contains(&result.token)
                        && (state.ready.len() >= MAX_READY || (!state.ready.is_empty() && state.ready_bytes.saturating_add(result.pixel_bytes) > MAX_READY_BYTES)) {
                        state = wake.wait(state).unwrap_or_else(|p| p.into_inner());
                    }
                    if state.stopping { break; }
                    if !state.pending.contains(&result.token) { continue; }
                    let post = state.ready.is_empty();
                    state.ready_bytes = state.ready_bytes.saturating_add(result.pixel_bytes);
                    state.ready.push_back(result);
                    post
                };
                if should_post && unsafe { PostMessageW(Some(HWND(target as *mut _)), message, WPARAM(0), LPARAM(0)) }.is_err() {
                    lock.lock().unwrap_or_else(|p| p.into_inner()).stop();
                    wake.notify_all();
                    break;
                }
            }
            if initialized { unsafe { CoUninitialize() }; }
        }).ok();
        if thread.is_none() { shared.0.lock().unwrap_or_else(|p| p.into_inner()).stop(); }
        Self { shared, thread }
    }

    /// False means the bounded queue is full; retry on the next repaint.
    pub fn request(&self, request: IconRequest) -> bool {
        let accepted = self.shared.0.lock().unwrap_or_else(|p| p.into_inner()).enqueue(request);
        if accepted { self.shared.1.notify_one(); }
        accepted
    }

    pub fn take_ready(&self) -> Vec<IconResult> {
        let mut state = self.shared.0.lock().unwrap_or_else(|p| p.into_inner());
        let ready: Vec<_> = state.ready.drain(..).collect();
        for result in &ready { state.pending.remove(&result.token); }
        state.ready_bytes = 0;
        self.shared.1.notify_all();
        ready
    }

    /// Cancels queued/stale work without interrupting a Shell extension mid-call.
    pub fn retain_requests(&self, tokens: &HashSet<u64>) {
        self.shared.0.lock().unwrap_or_else(|p| p.into_inner()).retain(tokens);
        self.shared.1.notify_all();
    }

    pub fn stats(&self) -> IconLoaderStats {
        let state = self.shared.0.lock().unwrap_or_else(|p| p.into_inner());
        IconLoaderStats { pending: state.pending.len(), queued: state.requests.len(), ready: state.ready.len(), ready_pixel_bytes: state.ready_bytes }
    }

    pub fn shutdown(&mut self) {
        self.shared.0.lock().unwrap_or_else(|p| p.into_inner()).stop();
        self.shared.1.notify_all();
        // Only the current extraction can remain; queued requests were discarded.
        if let Some(thread) = self.thread.take() { let _ = thread.join(); }
    }
}

impl Drop for IconLoader {
    fn drop(&mut self) { self.shutdown(); }
}

fn bitmap_bytes(bitmap: HBITMAP) -> u64 {
    if bitmap.is_invalid() { return 0; }
    let mut value = windows::Win32::Graphics::Gdi::BITMAP::default();
    let read = unsafe { GetObjectW(bitmap.into(), std::mem::size_of_val(&value) as i32, Some((&mut value as *mut windows::Win32::Graphics::Gdi::BITMAP).cast())) };
    if read <= 0 { 0 } else { u64::from(value.bmWidthBytes.unsigned_abs()) * u64::from(value.bmHeight.unsigned_abs()) }
}

fn load(request: &IconRequest) -> Option<(HBITMAP, BitmapAlpha)> {
    let item = super::pidl::item_from_pidl(&request.pidl).ok()?;
    let factory: IShellItemImageFactory = item.cast().ok()?;
    let size = SIZE { cx: request.size as i32, cy: request.size as i32 };
    let flags = if request.thumbnail { SIIGBF_RESIZETOFIT | SIIGBF_BIGGERSIZEOK } else { SIIGBF_ICONONLY | SIIGBF_BIGGERSIZEOK };
    let bitmap = unsafe { factory.GetImage(size, flags).ok()? };
    Some((bitmap, classify_alpha(bitmap)))
}

/// Shell bitmaps may carry straight, premultiplied or no alpha; inspect the pixels.
fn classify_alpha(bitmap: HBITMAP) -> BitmapAlpha {
    let mut section = DIBSECTION::default();
    let read = unsafe { GetObjectW(bitmap.into(), std::mem::size_of::<DIBSECTION>() as i32, Some((&mut section as *mut DIBSECTION).cast())) };
    if read as usize != std::mem::size_of::<DIBSECTION>() || section.dsBm.bmBits.is_null() || section.dsBm.bmBitsPixel != 32 {
        return BitmapAlpha::Ignore;
    }
    let len = (section.dsBm.bmWidthBytes as usize) * (section.dsBm.bmHeight.unsigned_abs() as usize);
    let pixels = unsafe { std::slice::from_raw_parts(section.dsBm.bmBits.cast::<u8>(), len) };
    let mut any_alpha = false;
    for px in pixels.chunks_exact(4) {
        let a = px[3];
        if a != 0 { any_alpha = true; }
        if px[0] > a || px[1] > a || px[2] > a { return if any_alpha || a != 0 { BitmapAlpha::Straight } else { scan_rest(pixels) }; }
    }
    if any_alpha { BitmapAlpha::Premultiplied } else { BitmapAlpha::Ignore }
}

fn scan_rest(pixels: &[u8]) -> BitmapAlpha {
    if pixels.chunks_exact(4).any(|p| p[3] != 0) { BitmapAlpha::Straight } else { BitmapAlpha::Ignore }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request(token: u64) -> IconRequest { IconRequest { key: "same-file".into(), pidl: vec![0,0], size: 48, thumbnail: true, token } }
    #[test]
    fn pending_queue_is_bounded_and_cancellation_frees_capacity() {
        let mut state = LoaderState::default();
        for token in 1..=MAX_PENDING as u64 { assert!(state.enqueue(request(token))); }
        assert!(!state.enqueue(request(999)));
        state.retain(&HashSet::from([1, 2]));
        assert_eq!(state.requests.len(), 2);
        assert!(state.enqueue(request(999)));
    }
    #[test]
    fn cached_file_can_be_requested_again_after_completion_or_cancellation() {
        let mut state = LoaderState::default();
        assert!(state.enqueue(request(1)));
        state.retain(&HashSet::new());
        assert!(state.enqueue(request(2)));
        state.stop();
        assert!(state.pending.is_empty() && state.requests.is_empty());
        assert!(!state.enqueue(request(3)));
    }
}
