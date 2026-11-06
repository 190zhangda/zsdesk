//! Modeless, read-only file preview with bounded local fallbacks.

use std::{cell::{Cell, RefCell}, fs::{self, File}, io::Read, mem::size_of,
    path::{Path, PathBuf}, ptr, rc::Rc};
use anyhow::{bail, Context, Result};
use windows::{core::{implement, Error, Interface, PCWSTR, PWSTR, HRESULT}, Win32::{
    Foundation::{COLORREF, ERROR_CLASS_ALREADY_EXISTS, GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM},
    Globalization::{MultiByteToWideChar, CP_ACP, MB_ERR_INVALID_CHARS},
    Graphics::{Gdi::*, Imaging::*},
    System::{Com::{CLSIDFromString, CoCreateInstance, IStream, CLSCTX_INPROC_SERVER, CLSCTX_LOCAL_SERVER,
        STGM_READ, STGM_SHARE_DENY_NONE}, LibraryLoader::GetModuleHandleW, Ole::{IObjectWithSite, IOleWindow, IOleWindow_Impl}},
    UI::{Controls::{EM_SETLIMITTEXT, EM_SETREADONLY}, HiDpi::{GetDpiForWindow, SystemParametersInfoForDpi},
        Input::KeyboardAndMouse::{VK_ESCAPE, VK_SPACE},
        Shell::{AssocQueryStringW, ASSOCF_NOTRUNCATE, ASSOCSTR_SHELLEXTENSION, IPreviewHandler,
            IPreviewHandlerFrame, IPreviewHandlerFrame_Impl, PREVIEWHANDLERFRAMEINFO, SHCreateStreamOnFileEx,
            PropertiesSystem::{IInitializeWithFile, IInitializeWithStream}}, WindowsAndMessaging::*},
}};

const MAX_HANDLER_BYTES: u64 = 128 * 1024 * 1024;
const MAX_TEXT_BYTES: usize = 256 * 1024;
const MAX_IMAGE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_IMAGE_PIXELS: u64 = 20_000_000;
const MAX_BITMAP_WIDTH: u32 = 1600;
const MAX_BITMAP_HEIGHT: u32 = 1200;
const CLASS_NAME: PCWSTR = windows::core::w!("ZSDesk.QuickPreview");

/// The owning Explorer window and this preview share one STA/message loop.
pub(super) struct QuickPreview { state: Rc<PreviewState> }

impl QuickPreview {
    pub(super) fn open(owner: HWND, path: &Path) -> Result<Self> {
        Self::open_impl(owner, path, true, true)
    }

    fn open_impl(owner: HWND, path: &Path, show: bool, allow_system_handler: bool) -> Result<Self> {
        let instance = HINSTANCE(unsafe { GetModuleHandleW(None)? }.0);
        let class = WNDCLASSW { lpfnWndProc: Some(window_proc), hInstance: instance,
            hCursor: unsafe { LoadCursorW(None, IDC_ARROW)? }, lpszClassName: CLASS_NAME, ..Default::default() };
        if unsafe { RegisterClassW(&class) } == 0 && unsafe { GetLastError() } != ERROR_CLASS_ALREADY_EXISTS {
            return Err(Error::from_thread().into());
        }
        let dark = zsdesk_windows::theme::system_dark_mode();
        let state = Rc::new(PreviewState { hwnd: Cell::new(HWND::default()), path: RefCell::new(PathBuf::new()),
            summary: RefCell::new(String::new()), notice: RefCell::new(String::new()), content: RefCell::new(Content::Empty),
            font: Cell::new(HFONT::default()), brush: unsafe { CreateSolidBrush(if dark { COLORREF(0x202020) } else { COLORREF(0xFFFFFF) }) },
            background: if dark { 0x202020 } else { 0xFFFFFF }, text: if dark { 0xF2F2F2 } else { 0x202020 }, dark, allow_system_handler });
        let bounds = initial_bounds(owner);
        let hwnd = unsafe { CreateWindowExW(WS_EX_TOOLWINDOW, CLASS_NAME, windows::core::w!("快速预览"),
            WS_CAPTION | WS_SYSMENU | WS_THICKFRAME | WS_CLIPCHILDREN,
            bounds.left, bounds.top, bounds.right - bounds.left, bounds.bottom - bounds.top,
            Some(owner), None, Some(instance), Some((&state as *const Rc<PreviewState>).cast()))? };
        state.hwnd.set(hwnd);
        state.update_font();
        zsdesk_windows::theme::set_dark_frame(hwnd, dark);
        let mut preview = Self { state };
        // Keeps arrow-key selection in Explorer available while the preview is open.
        if show { let _ = unsafe { ShowWindow(hwnd, SW_SHOWNOACTIVATE) }; }
        preview.update(path)?;
        Ok(preview)
    }

    pub(super) fn is_open(&self) -> bool {
        let hwnd = self.state.hwnd.get();
        !hwnd.0.is_null() && unsafe { IsWindow(Some(hwnd)) }.as_bool()
    }

    pub(super) fn owns_window(&self, hwnd: HWND) -> bool {
        let preview = self.state.hwnd.get();
        !preview.0.is_null() && (hwnd == preview || unsafe { IsChild(preview, hwnd) }.as_bool())
    }

    pub(super) fn update(&mut self, path: &Path) -> Result<()> {
        if !self.is_open() || self.state.path.borrow().as_path() == path { return Ok(()); }
        self.state.release_content();
        if !self.is_open() { return Ok(()); }
        *self.state.path.borrow_mut() = path.to_path_buf();
        self.state.notice.borrow_mut().clear();
        let title = format!("{} — 快速预览", path.file_name().unwrap_or(path.as_os_str()).to_string_lossy());
        let _ = unsafe { SetWindowTextW(self.state.hwnd.get(), PCWSTR(wide(&title).as_ptr())) };
        let metadata = match fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(error) => {
                self.state.summary.borrow_mut().clear();
                *self.state.notice.borrow_mut() = format!("无法读取此文件。\r\n{error}");
                self.state.invalidate();
                return Ok(());
            }
        };
        *self.state.summary.borrow_mut() = if metadata.is_dir() { "文件夹".into() }
            else { format!("{} · {}", file_type(path), zsdesk_ui::chrome::format_size(metadata.len())) };
        if metadata.is_dir() {
            *self.state.notice.borrow_mut() = "请选择文件以显示内容预览。".into();
            self.state.invalidate();
            return Ok(());
        }
        let hwnd = self.state.hwnd.get();
        let rect = self.state.content_rect();
        let content = if self.state.allow_system_handler && metadata.len() <= MAX_HANDLER_BYTES {
            system_preview(hwnd, path, rect).ok().map(Content::Handler)
        } else { None };
        let content = content.or_else(|| {
            if self.state.hwnd.get() != hwnd || !unsafe { IsWindow(Some(hwnd)) }.as_bool() { return None; }
            if is_text_path(path) {
                read_text(path).ok().and_then(|text| self.state.text_control(&text).ok()).map(Content::Text)
            } else if is_image_path(path) && metadata.len() <= MAX_IMAGE_BYTES {
                decode_image(path, self.state.background).ok().map(Content::Image)
            } else { None }
        });
        // Out-of-process COM calls may pump messages, including closing this window.
        if self.state.hwnd.get() != hwnd || !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
            drop(content);
            return Ok(());
        }
        match content {
            Some(content) => { *self.state.content.borrow_mut() = content; }
            None => {
                *self.state.notice.borrow_mut() = if metadata.len() > MAX_HANDLER_BYTES {
                    "文件较大，快速预览仅显示文件信息。".into()
                } else { "此文件暂无可用的内容预览。".into() };
            }
        }
        self.state.layout_content();
        self.state.invalidate();
        Ok(())
    }

    /// Called before the Explorer accelerator table for messages owned by this preview.
    pub(super) fn pre_translate(&mut self, message: &MSG) -> bool {
        if !self.owns_window(message.hwnd) { return false; }
        if closes_preview(message) { self.close(); return true; }
        let handler = self.state.handler();
        handler.is_some_and(|handler| unsafe {
            (Interface::vtable(&handler).TranslateAccelerator)(Interface::as_raw(&handler), message) == HRESULT(0)
        })
    }

    pub(super) fn close(&mut self) {
        let hwnd = self.state.hwnd.get();
        if !hwnd.0.is_null() && unsafe { IsWindow(Some(hwnd)) }.as_bool() { let _ = unsafe { DestroyWindow(hwnd) }; }
        self.state.hwnd.set(HWND::default());
        self.state.release_content();
    }

    #[cfg(test)]
    #[allow(dead_code)] // Invoked by the isolated native-host probe.
    pub(super) fn open_hidden(owner: HWND, path: &Path) -> Result<Self> { Self::open_impl(owner, path, false, true) }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(super) fn open_hidden_fallback(owner: HWND, path: &Path) -> Result<Self> { Self::open_impl(owner, path, false, false) }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(super) fn snapshot(&self) -> PreviewSnapshot {
        let (kind, bytes, child) = match &*self.state.content.borrow() {
            Content::Empty => ("information", 0, None),
            Content::Handler(_) => ("system", 0, None),
            Content::Text(text) => ("text", 0, Some(text.0)),
            Content::Image(image) => ("image", image.pixels.len(), None),
        };
        PreviewSnapshot { hwnd: self.state.hwnd.get(), kind, image_bytes: bytes, text_child: child,
            notice: self.state.notice.borrow().clone(), summary: self.state.summary.borrow().clone() }
    }
}

#[cfg(test)]
#[derive(Debug)]
#[allow(dead_code)]
pub(super) struct PreviewSnapshot {
    pub hwnd: HWND, pub kind: &'static str, pub image_bytes: usize, pub text_child: Option<HWND>,
    pub notice: String, pub summary: String,
}

impl Drop for QuickPreview { fn drop(&mut self) { self.close(); } }

struct PreviewState {
    hwnd: Cell<HWND>, path: RefCell<PathBuf>, summary: RefCell<String>, notice: RefCell<String>,
    content: RefCell<Content>, font: Cell<HFONT>, brush: HBRUSH, background: u32, text: u32, dark: bool,
    allow_system_handler: bool,
}

enum Content { Empty, Handler(SystemPreview), Text(TextPreview), Image(ImagePreview) }
struct TextPreview(HWND);
impl Drop for TextPreview {
    fn drop(&mut self) { if unsafe { IsWindow(Some(self.0)) }.as_bool() { let _ = unsafe { DestroyWindow(self.0) }; } }
}
struct ImagePreview { width: u32, height: u32, pixels: Vec<u8> }
struct SystemPreview { handler: IPreviewHandler, _stream: Option<IStream>, _site: IPreviewHandlerFrame }
impl Drop for SystemPreview {
    fn drop(&mut self) {
        let _ = unsafe { self.handler.Unload() };
        if let Ok(site) = self.handler.cast::<IObjectWithSite>() { let _ = unsafe { site.SetSite(None) }; }
    }
}

impl PreviewState {
    fn s(&self, value: i32) -> i32 { zsdesk_ui::chrome::scale(value, unsafe { GetDpiForWindow(self.hwnd.get()) }.max(96)) }
    fn release_content(&self) { let old = self.content.replace(Content::Empty); drop(old); }
    fn handler(&self) -> Option<IPreviewHandler> {
        match &*self.content.borrow() { Content::Handler(preview) => Some(preview.handler.clone()), _ => None }
    }
    fn content_rect(&self) -> RECT {
        let mut rect = RECT::default();
        let _ = unsafe { GetClientRect(self.hwnd.get(), &mut rect) };
        let margin = self.s(12);
        RECT { left: margin, top: self.s(54), right: (rect.right - margin).max(margin), bottom: (rect.bottom - self.s(32)).max(self.s(54)) }
    }
    fn invalidate(&self) {
        let hwnd = self.hwnd.get();
        if !hwnd.0.is_null() { let _ = unsafe { InvalidateRect(Some(hwnd), None, false) }; }
    }
    fn layout_content(&self) {
        let rect = self.content_rect();
        let handler = self.handler();
        let text = match &*self.content.borrow() { Content::Text(text) => Some(text.0), _ => None };
        if let Some(handler) = handler { let _ = unsafe { handler.SetRect(&rect) }; }
        if let Some(hwnd) = text { let _ = unsafe { MoveWindow(hwnd, rect.left, rect.top, rect.right - rect.left, rect.bottom - rect.top, true) }; }
    }
    fn update_font(&self) {
        let mut metrics = NONCLIENTMETRICSW { cbSize: size_of::<NONCLIENTMETRICSW>() as u32, ..Default::default() };
        if unsafe { SystemParametersInfoForDpi(SPI_GETNONCLIENTMETRICS.0, metrics.cbSize,
            Some((&mut metrics as *mut NONCLIENTMETRICSW).cast()), 0, GetDpiForWindow(self.hwnd.get()).max(96)) }.is_ok() {
            let next = unsafe { CreateFontIndirectW(&metrics.lfMessageFont) };
            if !next.0.is_null() {
                let previous = self.font.replace(next);
                if let Content::Text(text) = &*self.content.borrow() {
                    unsafe { SendMessageW(text.0, WM_SETFONT, Some(WPARAM(next.0 as usize)), Some(LPARAM(1))); }
                }
                if !previous.0.is_null() { let _ = unsafe { DeleteObject(previous.into()) }; }
            }
        }
    }
    fn text_control(&self, text: &str) -> Result<TextPreview> {
        let rect = self.content_rect();
        let text = wide(text);
        let child = unsafe { CreateWindowExW(WINDOW_EX_STYLE::default(), windows::core::w!("EDIT"), PCWSTR(text.as_ptr()),
            WS_CHILD | WS_VISIBLE | WS_VSCROLL | WS_HSCROLL | WINDOW_STYLE((ES_MULTILINE | ES_READONLY | ES_AUTOVSCROLL | ES_AUTOHSCROLL) as u32),
            rect.left, rect.top, rect.right - rect.left, rect.bottom - rect.top, Some(self.hwnd.get()), None, None, None)? };
        zsdesk_windows::theme::theme_control(child, self.dark);
        unsafe {
            SendMessageW(child, EM_SETREADONLY, Some(WPARAM(1)), None);
            SendMessageW(child, EM_SETLIMITTEXT, Some(WPARAM(MAX_TEXT_BYTES * 2)), None);
            SendMessageW(child, WM_SETFONT, Some(WPARAM(self.font.get().0 as usize)), Some(LPARAM(1)));
        }
        Ok(TextPreview(child))
    }
    fn paint(&self) {
        let hwnd = self.hwnd.get();
        let mut paint = PAINTSTRUCT::default();
        let dc = unsafe { BeginPaint(hwnd, &mut paint) };
        let mut client = RECT::default();
        let _ = unsafe { GetClientRect(hwnd, &mut client) };
        unsafe { FillRect(dc, &client, self.brush); SetBkMode(dc, TRANSPARENT); SetTextColor(dc, COLORREF(self.text)); }
        let old_font = if !self.font.get().0.is_null() { Some(unsafe { SelectObject(dc, self.font.get().into()) }) } else { None };
        let mut header = RECT { left: self.s(12), top: self.s(10), right: client.right - self.s(12), bottom: self.s(42) };
        draw_text(dc, &self.summary.borrow(), &mut header, DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX);
        let mut footer = RECT { left: self.s(12), top: (client.bottom - self.s(28)).max(0), right: client.right - self.s(12), bottom: client.bottom };
        draw_text(dc, "空格 / Esc 关闭", &mut footer, DT_SINGLELINE | DT_VCENTER | DT_RIGHT | DT_NOPREFIX);
        let content = self.content_rect();
        match &*self.content.borrow() {
            Content::Image(image) => {
                let (w, h) = fit_dimensions(image.width, image.height, (content.right - content.left).max(1) as u32, (content.bottom - content.top).max(1) as u32);
                let x = content.left + ((content.right - content.left) - w as i32) / 2;
                let y = content.top + ((content.bottom - content.top) - h as i32) / 2;
                let bitmap = BITMAPINFO { bmiHeader: BITMAPINFOHEADER { biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: image.width as i32, biHeight: -(image.height as i32), biPlanes: 1, biBitCount: 32,
                    biCompression: BI_RGB.0, ..Default::default() }, ..Default::default() };
                unsafe { SetStretchBltMode(dc, HALFTONE); StretchDIBits(dc, x, y, w as i32, h as i32, 0, 0,
                    image.width as i32, image.height as i32, Some(image.pixels.as_ptr().cast()), &bitmap, DIB_RGB_COLORS, SRCCOPY); }
            }
            Content::Empty => { let mut area = content; draw_text(dc, &self.notice.borrow(), &mut area, DT_WORDBREAK | DT_NOPREFIX); }
            _ => {}
        }
        if let Some(font) = old_font { unsafe { SelectObject(dc, font); } }
        let _ = unsafe { EndPaint(hwnd, &paint) };
    }
}

impl Drop for PreviewState {
    fn drop(&mut self) {
        self.release_content();
        if !self.font.get().0.is_null() { let _ = unsafe { DeleteObject(self.font.get().into()) }; }
        if !self.brush.0.is_null() { let _ = unsafe { DeleteObject(self.brush.into()) }; }
    }
}

unsafe extern "system" fn window_proc(hwnd: HWND, message: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if message == WM_NCCREATE {
        let create = unsafe { &*(lp.0 as *const CREATESTRUCTW) };
        let state = unsafe { &*(create.lpCreateParams as *const Rc<PreviewState>) }.clone();
        state.hwnd.set(hwnd);
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, Rc::into_raw(state) as isize); }
        return LRESULT(1);
    }
    let raw = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const PreviewState;
    if raw.is_null() { return unsafe { DefWindowProcW(hwnd, message, wp, lp) }; }
    unsafe { Rc::increment_strong_count(raw); }
    let state = unsafe { Rc::from_raw(raw) };
    match message {
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => { state.paint(); LRESULT(0) }
        WM_GETMINMAXINFO => {
            if lp.0 != 0 {
                let info = unsafe { &mut *(lp.0 as *mut MINMAXINFO) };
                info.ptMinTrackSize.x = state.s(320);
                info.ptMinTrackSize.y = state.s(220);
            }
            LRESULT(0)
        }
        WM_CTLCOLORSTATIC | WM_CTLCOLOREDIT => {
            let text = matches!(&*state.content.borrow(), Content::Text(text) if text.0.0 as isize == lp.0);
            if text {
                let dc = HDC(wp.0 as *mut _);
                unsafe { SetTextColor(dc, COLORREF(state.text)); SetBkColor(dc, COLORREF(state.background)); }
                LRESULT(state.brush.0 as isize)
            } else { unsafe { DefWindowProcW(hwnd, message, wp, lp) } }
        }
        WM_SIZE => { state.layout_content(); state.invalidate(); LRESULT(0) }
        WM_DPICHANGED => {
            let rect = unsafe { &*(lp.0 as *const RECT) };
            let _ = unsafe { SetWindowPos(hwnd, None, rect.left, rect.top, rect.right - rect.left, rect.bottom - rect.top, SWP_NOACTIVATE | SWP_NOZORDER) };
            state.update_font(); state.layout_content(); LRESULT(0)
        }
        WM_KEYDOWN if wp.0 == VK_ESCAPE.0 as usize || wp.0 == VK_SPACE.0 as usize => {
            let _ = unsafe { PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)) }; LRESULT(0)
        }
        WM_CLOSE => { let _ = unsafe { DestroyWindow(hwnd) }; LRESULT(0) }
        WM_DESTROY => { state.hwnd.set(HWND::default()); state.release_content(); LRESULT(0) }
        WM_NCDESTROY => {
            unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0); drop(Rc::from_raw(raw)); }
            unsafe { DefWindowProcW(hwnd, message, wp, lp) }
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wp, lp) },
    }
}

#[implement(IPreviewHandlerFrame, IOleWindow)]
struct PreviewFrame { hwnd: isize }
#[allow(non_snake_case)]
impl IPreviewHandlerFrame_Impl for PreviewFrame_Impl {
    fn GetWindowContext(&self) -> windows::core::Result<PREVIEWHANDLERFRAMEINFO> { Ok(PREVIEWHANDLERFRAMEINFO::default()) }
    fn TranslateAccelerator(&self, message: *const MSG) -> windows::core::Result<()> {
        if !message.is_null() && closes_preview(unsafe { &*message }) {
            unsafe { PostMessageW(Some(HWND(self.hwnd as *mut _)), WM_CLOSE, WPARAM(0), LPARAM(0))?; }
            Ok(())
        } else { Err(Error::from_hresult(HRESULT(1))) }
    }
}
#[allow(non_snake_case)]
impl IOleWindow_Impl for PreviewFrame_Impl {
    fn GetWindow(&self) -> windows::core::Result<HWND> { Ok(HWND(self.hwnd as *mut _)) }
    fn ContextSensitiveHelp(&self, _enter: windows::core::BOOL) -> windows::core::Result<()> { Err(Error::from_hresult(HRESULT(0x80004001u32 as i32))) }
}

fn closes_preview(message: &MSG) -> bool {
    message.message == WM_KEYDOWN && (message.wParam.0 == VK_ESCAPE.0 as usize || message.wParam.0 == VK_SPACE.0 as usize)
}

fn system_preview(hwnd: HWND, path: &Path, rect: RECT) -> Result<SystemPreview> {
    let clsid = preview_handler_clsid(path)?;
    // A registered surrogate/local server owns document code and its resources.
    let handler: IPreviewHandler = unsafe { CoCreateInstance(&clsid, None, CLSCTX_LOCAL_SERVER) }.context("无法启动进程外预览器")?;
    let site: IPreviewHandlerFrame = PreviewFrame { hwnd: hwnd.0 as isize }.into();
    let mut preview = SystemPreview { handler, _stream: None, _site: site };
    let with_site = preview.handler.cast::<IObjectWithSite>().context("预览器未提供宿主连接")?;
    unsafe { with_site.SetSite(&preview._site) }.context("预览器连接宿主失败")?;
    match preview.handler.cast::<IInitializeWithStream>() {
        Ok(initialize) => {
            let stream = read_stream(path)?;
            unsafe { initialize.Initialize(&stream, STGM_READ.0) }.context("预览器只读流初始化失败")?;
            preview._stream = Some(stream);
        }
        Err(error) if error.code() == HRESULT(0x80004002u32 as i32) => {
            let initialize = preview.handler.cast::<IInitializeWithFile>().context("预览器未提供只读文件接口")?;
            let file = wide_path(path);
            unsafe { initialize.Initialize(PCWSTR(file.as_ptr()), STGM_READ.0) }.context("预览器只读文件初始化失败")?;
        }
        Err(error) => return Err(error.into()),
    }
    unsafe { preview.handler.SetWindow(hwnd, &rect) }.context("预览器绑定窗口失败")?;
    unsafe { preview.handler.DoPreview() }.context("预览器加载内容失败")?;
    Ok(preview)
}

fn preview_handler_clsid(path: &Path) -> Result<windows::core::GUID> {
    let extension = path.extension().and_then(|v| v.to_str()).context("文件没有预览类型")?;
    let extension = wide(&format!(".{extension}"));
    let iid = windows::core::w!("{8895B1C6-B41F-4C1C-A562-0D564250836F}");
    let mut length = 0;
    let _ = unsafe { AssocQueryStringW(ASSOCF_NOTRUNCATE, ASSOCSTR_SHELLEXTENSION, PCWSTR(extension.as_ptr()), iid, None, &mut length) };
    if !(2..=1024).contains(&length) { bail!("没有已注册的预览器"); }
    let mut clsid = vec![0u16; length as usize];
    unsafe { AssocQueryStringW(ASSOCF_NOTRUNCATE, ASSOCSTR_SHELLEXTENSION, PCWSTR(extension.as_ptr()), iid, Some(PWSTR(clsid.as_mut_ptr())), &mut length).ok()?; }
    Ok(unsafe { CLSIDFromString(PCWSTR(clsid.as_ptr()))? })
}

#[cfg(test)]
#[allow(dead_code)]
pub(super) fn handler_registration_for_test(path: &Path) -> Result<String> { Ok(format!("{:?}", preview_handler_clsid(path)?)) }

#[cfg(test)]
#[allow(dead_code)]
pub(super) fn handler_open_for_test(owner: HWND, path: &Path) -> Result<()> {
    let handler = system_preview(owner, path, RECT { left: 0, top: 0, right: 400, bottom: 300 })?;
    drop(handler);
    Ok(())
}

fn read_stream(path: &Path) -> windows::core::Result<IStream> {
    let name = wide_path(path);
    unsafe { SHCreateStreamOnFileEx(PCWSTR(name.as_ptr()), (STGM_READ | STGM_SHARE_DENY_NONE).0, 0, false, None) }
}

fn read_text(path: &Path) -> Result<String> {
    let mut bytes = Vec::new();
    File::open(path)?.take((MAX_TEXT_BYTES + 1) as u64).read_to_end(&mut bytes)?;
    let truncated = bytes.len() > MAX_TEXT_BYTES;
    bytes.truncate(MAX_TEXT_BYTES);
    let mut text = decode_text(&bytes).context("文件不是可显示的文本")?;
    if truncated { text.push_str("\r\n\r\n…仅显示前 256 KB"); }
    Ok(text)
}

fn decode_text(bytes: &[u8]) -> Option<String> {
    if bytes.starts_with(&[0xFF, 0xFE]) || bytes.starts_with(&[0xFE, 0xFF]) {
        let little = bytes[0] == 0xFF;
        let values: Vec<u16> = bytes[2..].chunks_exact(2).map(|v| if little { u16::from_le_bytes([v[0], v[1]]) } else { u16::from_be_bytes([v[0], v[1]]) }).collect();
        return Some(String::from_utf16_lossy(&values).replace('\0', "�"));
    }
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    if bytes.contains(&0) { return None; }
    match std::str::from_utf8(bytes) {
        Ok(text) => return Some(text.to_string()),
        Err(error) if error.error_len().is_none() => return Some(String::from_utf8_lossy(&bytes[..error.valid_up_to()]).into_owned()),
        Err(_) => {}
    }
    if bytes.is_empty() { return Some(String::new()); }
    let length = unsafe { MultiByteToWideChar(CP_ACP, MB_ERR_INVALID_CHARS, bytes, None) };
    if length <= 0 { return Some(String::from_utf8_lossy(bytes).into_owned()); }
    let mut text = vec![0u16; length as usize];
    let converted = unsafe { MultiByteToWideChar(CP_ACP, MB_ERR_INVALID_CHARS, bytes, Some(&mut text)) };
    (converted > 0).then(|| String::from_utf16_lossy(&text[..converted as usize]))
}

fn decode_image(path: &Path, background: u32) -> Result<ImagePreview> {
    let factory: IWICImagingFactory = unsafe { CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)? };
    let stream = read_stream(path)?;
    let decoder = unsafe { factory.CreateDecoderFromStream(&stream, ptr::null(), WICDecodeMetadataCacheOnDemand)? };
    let frame = unsafe { decoder.GetFrame(0)? };
    let (mut width, mut height) = (0, 0);
    unsafe { frame.GetSize(&mut width, &mut height)?; }
    if !image_dimensions_allowed(width, height) { bail!("图片像素尺寸超出快速预览范围"); }
    let (target_w, target_h) = fit_dimensions(width, height, MAX_BITMAP_WIDTH, MAX_BITMAP_HEIGHT);
    let scaled: IWICBitmapSource = if (target_w, target_h) != (width, height) {
        let scaler = unsafe { factory.CreateBitmapScaler()? };
        unsafe { scaler.Initialize(&frame, target_w, target_h, WICBitmapInterpolationModeFant)?; }
        scaler.cast()?
    } else { frame.cast()? };
    let converter = unsafe { factory.CreateFormatConverter()? };
    unsafe { converter.Initialize(&scaled, &GUID_WICPixelFormat32bppBGRA, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeCustom)?; }
    let stride = target_w.checked_mul(4).context("图片宽度无效")?;
    let bytes = stride.checked_mul(target_h).context("图片尺寸无效")?;
    let mut pixels = vec![0u8; bytes as usize];
    unsafe { converter.CopyPixels(ptr::null(), stride, &mut pixels)?; }
    let bg = [(background >> 16) as u8, (background >> 8) as u8, background as u8];
    for pixel in pixels.chunks_exact_mut(4) {
        let alpha = u32::from(pixel[3]);
        for channel in 0..3 { pixel[channel] = ((u32::from(pixel[channel]) * alpha + u32::from(bg[channel]) * (255 - alpha) + 127) / 255) as u8; }
        pixel[3] = 255;
    }
    Ok(ImagePreview { width: target_w, height: target_h, pixels })
}

fn image_dimensions_allowed(width: u32, height: u32) -> bool {
    width > 0 && height > 0 && u64::from(width) * u64::from(height) <= MAX_IMAGE_PIXELS
}
fn fit_dimensions(width: u32, height: u32, max_width: u32, max_height: u32) -> (u32, u32) {
    if width == 0 || height == 0 || max_width == 0 || max_height == 0 { return (0, 0); }
    let ratio = (max_width as f64 / width as f64).min(max_height as f64 / height as f64).min(1.0);
    ((width as f64 * ratio).round().max(1.0) as u32, (height as f64 * ratio).round().max(1.0) as u32)
}
fn is_text_path(path: &Path) -> bool {
    matches!(extension(path).as_str(), "txt"|"md"|"log"|"csv"|"tsv"|"json"|"toml"|"ini"|"cfg"|"xml"|"yaml"|"yml"|
        "rs"|"c"|"h"|"cpp"|"hpp"|"py"|"js"|"ts"|"css"|"html"|"htm"|"sql"|"ps1"|"bat"|"cmd"|"sh")
}
fn is_image_path(path: &Path) -> bool { matches!(extension(path).as_str(), "png"|"jpg"|"jpeg"|"bmp"|"gif"|"tif"|"tiff"|"webp"|"ico") }
fn extension(path: &Path) -> String { path.extension().unwrap_or_default().to_string_lossy().to_ascii_lowercase() }
fn file_type(path: &Path) -> String { let ext = extension(path); if ext.is_empty() { "文件".into() } else { format!("{} 文件", ext.to_uppercase()) } }
fn wide(value: &str) -> Vec<u16> { value.encode_utf16().chain(Some(0)).collect() }
fn wide_path(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}
fn draw_text(dc: HDC, text: &str, rect: &mut RECT, format: DRAW_TEXT_FORMAT) {
    let mut text: Vec<u16> = text.encode_utf16().collect();
    unsafe { DrawTextW(dc, &mut text, rect, format); }
}
fn initial_bounds(owner: HWND) -> RECT {
    let dpi = unsafe { GetDpiForWindow(owner) }.max(96);
    let scale = |value| zsdesk_ui::chrome::scale(value, dpi);
    let monitor = unsafe { MonitorFromWindow(owner, MONITOR_DEFAULTTONEAREST) };
    let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
    let mut owner_rect = RECT::default();
    let _ = unsafe { GetWindowRect(owner, &mut owner_rect) };
    let work = if unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() { info.rcWork }
        else { RECT { left: 0, top: 0, right: 1024, bottom: 768 } };
    let width = scale(760).min((work.right - work.left - scale(32)).max(1));
    let height = scale(600).min((work.bottom - work.top - scale(32)).max(1));
    let x = (owner_rect.left + (owner_rect.right - owner_rect.left - width) / 2).clamp(work.left, (work.right - width).max(work.left));
    let y = (owner_rect.top + (owner_rect.bottom - owner_rect.top - height) / 2).clamp(work.top, (work.bottom - height).max(work.top));
    RECT { left: x, top: y, right: x + width, bottom: y + height }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn text_decoding_preserves_unicode_and_rejects_binary_nuls() {
        assert_eq!(decode_text("施工记录".as_bytes()).unwrap(), "施工记录");
        assert_eq!(decode_text(&[0xEF,0xBB,0xBF,b'A']).unwrap(), "A");
        assert_eq!(decode_text(&[0xFF,0xFE,0x2D,0x4E,0x87,0x65]).unwrap(), "中文");
        assert_eq!(decode_text(&[0xFE,0xFF,0x4E,0x2D,0x65,0x87]).unwrap(), "中文");
        assert_eq!(decode_text(&"中文".as_bytes()[..5]).unwrap(), "中");
        assert!(decode_text(&[1,0,2,0]).is_none());
    }
    #[test]
    fn image_limits_prevent_overflow_and_bound_the_retained_bitmap() {
        assert!(!image_dimensions_allowed(0,100));
        assert!(!image_dimensions_allowed(u32::MAX,u32::MAX));
        assert!(!image_dimensions_allowed(10000,10000));
        let (w,h) = fit_dimensions(6000,3000,MAX_BITMAP_WIDTH,MAX_BITMAP_HEIGHT);
        assert_eq!((w,h),(1600,800));
        assert!(u64::from(w)*u64::from(h)*4 <= u64::from(MAX_BITMAP_WIDTH)*u64::from(MAX_BITMAP_HEIGHT)*4);
        assert_eq!(fit_dimensions(100,100,1600,1200),(100,100));
    }
    #[test]
    fn executable_files_are_not_treated_as_text_or_images() {
        assert!(!is_text_path(Path::new("工具.exe")) && !is_image_path(Path::new("工具.exe")));
        assert!(is_text_path(Path::new("日志.TXT")) && is_image_path(Path::new("图纸.PNG")));
    }
}
