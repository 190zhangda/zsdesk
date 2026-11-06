//! Native Shell context menus (`IContextMenu`) hosted by ZSDesk windows.
//!
//! While a menu is tracked, owner-drawn and lazily populated submenus
//! ("新建", "发送到", "打开方式") need `WM_INITMENUPOPUP`, `WM_DRAWITEM`,
//! `WM_MEASUREITEM` and `WM_MENUCHAR` forwarded to `IContextMenu2/3`. Every
//! window procedure that can own a menu calls [`forward_menu_message`] first.

use std::{cell::{Cell, RefCell}, rc::Rc};

use windows::{
    core::{Interface, PCSTR, PCWSTR, PSTR, Result},
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM},
        System::{Com::CoTaskMemFree, Ole::IOleWindow},
        UI::{
            Shell::{
                Common::ITEMIDLIST, IContextMenu, IContextMenu2, IContextMenu3, IShellFolder, IShellView,
                ILClone, ILFindLastID, ILRemoveLastID, SHBindToParent, SVGIO_BACKGROUND, SVGIO_SELECTION,
                CMINVOKECOMMANDINFO, CMINVOKECOMMANDINFOEX, CMIC_MASK_PTINVOKE,
                CMIC_MASK_SHIFT_DOWN, CMIC_MASK_CONTROL_DOWN, CMF_NORMAL, CMF_CANRENAME, CMF_EXTENDEDVERBS,
                CMF_EXPLORE, CMF_ASYNCVERBSTATE, CMF_OPTIMIZEFORINVOKE, GCS_VERBW, IFolderView2, DefSubclassProc, SetWindowSubclass, RemoveWindowSubclass, GetWindowSubclass,
            },
            WindowsAndMessaging::{
                CreatePopupMenu, DestroyMenu, GetMenuItemCount, GetMenuItemID, GetSubMenu, TrackPopupMenuEx,
                HMENU, SW_SHOWNORMAL, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_DRAWITEM, WM_INITMENUPOPUP,
                WM_MEASUREITEM, WM_MENUCHAR, TPM_LEFTALIGN, TPM_TOPALIGN, TRACK_POPUP_MENU_FLAGS,
                GetMenuStringW, GetMenuState, DeleteMenu, MF_BYPOSITION, MF_SEPARATOR,
                WM_CONTEXTMENU, WM_NCDESTROY, GetCursorPos, GetWindowRect,
            },
            Input::KeyboardAndMouse::{GetKeyState, VK_SHIFT, VK_CONTROL},
        },
    },
};

/// First and last command identifiers reserved for Shell items.
pub const SHELL_FIRST: u32 = 1;
pub const SHELL_LAST: u32 = 0x6FFF;
/// Application items appended to a Shell menu should use ids from here.
pub const APP_FIRST: u32 = 0x7000;

/// `SEE_MASK_UNICODE`, shared by CMINVOKECOMMANDINFOEX.
const CMIC_MASK_UNICODE: u32 = 0x4000;

#[derive(Clone)]
struct ActiveContext { menu: IContextMenu, filter: bool }
thread_local! {
    static ACTIVE: RefCell<Option<ActiveContext>> = const { RefCell::new(None) };
    static OPTIONS: RefCell<ContextMenuOptions> = RefCell::new(ContextMenuOptions::default());
}

/// Preferences affect only this application's menus. Both native presentations
/// retain Shell command identities while hiding the selected functions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextMenuOptions {
    pub modern: bool,
    pub hidden_verbs: Vec<String>,
    pub show_seven_zip: bool,
    pub show_wps: bool,
}
impl Default for ContextMenuOptions {
    fn default() -> Self { Self { modern: true, hidden_verbs: Vec::new(), show_seven_zip: true, show_wps: false } }
}
pub fn set_context_menu_options(mut options: ContextMenuOptions) {
    options.hidden_verbs.retain(|v| !v.is_empty() && v.len() <= 128 && !v.contains('\0'));
    for verb in &mut options.hidden_verbs { verb.make_ascii_lowercase(); }
    options.hidden_verbs.sort(); options.hidden_verbs.dedup();
    #[cfg(all(target_arch = "x86_64", target_env = "msvc"))]
    unsafe {
        zs_menu_set_hidden_features(modern_hidden_features(&options));
        zs_menu_set_original_callback(Some(show_expanded_view_menu));
    }
    OPTIONS.with(|current| *current.borrow_mut() = options);
}
pub fn context_menu_options() -> ContextMenuOptions { OPTIONS.with(|options| options.borrow().clone()) }

/// Posted to the containing application window when a filtered menu cannot be
/// opened. WPARAM contains the HRESULT; no unfiltered fallback is displayed.
pub fn failure_message() -> u32 {
    static MESSAGE: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *MESSAGE.get_or_init(|| unsafe {
        windows::Win32::UI::WindowsAndMessaging::RegisterWindowMessageW(windows::core::w!("ZSDesk.ContextMenuFailure"))
    })
}

#[cfg(all(target_arch = "x86_64", target_env = "msvc"))]
extern "C" {
    fn zs_menu_set_hidden_features(hidden: u32);
    fn zs_menu_set_original_callback(callback: Option<unsafe extern "system" fn(*mut core::ffi::c_void, POINT) -> windows::core::HRESULT>);
}

// Shared bit contract with native/MenuModelFilter.h. Unknown canonical verbs
// remain usable by the original menu without guessing a private XAML identity.
fn modern_hidden_features(options: &ContextMenuOptions) -> u32 {
    let mut hidden = 0;
    for verb in &options.hidden_verbs { hidden |= command_feature(verb); }
    if !options.show_seven_zip { hidden |= 1 << 10; }
    if !options.show_wps { hidden |= 1 << 11; }
    hidden
}
fn command_feature(verb: &str) -> u32 {
    match verb {
        "cut" | "windows.cut" => 1 << 0, "copy" | "windows.copy" => 1 << 1,
        "paste" | "windows.paste" => 1 << 2, "pastelink" | "windows.pastelink" => 1 << 3,
        "copyaspath" | "copypath" | "windows.copyaspath" => 1 << 4,
        "rename" | "windows.rename" => 1 << 5, "delete" | "windows.delete" => 1 << 6,
        "properties" | "windows.properties" => 1 << 7,
        "share" | "windows.share" | "windows.modernshare" | "windows.modernshareflyout" => 1 << 8,
        "new" | "newfolder" | "windows.new" | "windows.newfolder" => 1 << 9,
        _ => 0,
    }
}

struct ActiveMenuScope(Option<ActiveContext>);
impl ActiveMenuScope {
    fn enter(menu: &IContextMenu, filter: bool) -> Self { Self(ACTIVE.with(|active| active.replace(Some(ActiveContext { menu: menu.clone(), filter })))) }
}
impl Drop for ActiveMenuScope {
    fn drop(&mut self) { ACTIVE.with(|active| *active.borrow_mut() = self.0.take()); }
}

/// Forwards menu messages to the context menu being tracked on this thread.
pub fn forward_menu_message(msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
    if ![WM_INITMENUPOPUP, WM_DRAWITEM, WM_MEASUREITEM, WM_MENUCHAR].contains(&msg) { return None; }
    let active = ACTIVE.with(|active| active.borrow().clone())?;
    let menu = active.menu;
    let mut handled = None;
    if let Ok(menu3) = menu.cast::<IContextMenu3>() {
        let mut result = LRESULT(0);
        if unsafe { menu3.HandleMenuMsg2(msg, wp, lp, Some(&mut result)) }.is_ok() { handled = Some(result); }
    } else if let Ok(menu2) = menu.cast::<IContextMenu2>() {
        if unsafe { menu2.HandleMenuMsg(msg, wp, lp) }.is_ok() { handled = Some(LRESULT(0)); }
    }
    if msg == WM_INITMENUPOPUP && active.filter {
        filter_native_menu(HMENU(wp.0 as *mut _), &menu, &context_menu_options());
    }
    handled
}

pub struct ShellMenu {
    menu: IContextMenu,
    hmenu: HMENU,
}

impl ShellMenu {
    pub fn new(menu: IContextMenu) -> Result<Self> {
        let hmenu = unsafe { CreatePopupMenu()? };
        Ok(Self { menu, hmenu })
    }

    /// Context menu for items that share one parent folder (absolute PIDL bytes).
    pub fn for_items(owner: HWND, pidls: &[Vec<u8>]) -> Result<Self> {
        let first = pidls.first().ok_or_else(|| windows::core::Error::from_hresult(windows::core::HRESULT(0x80070057u32 as i32)))?;
        let mut last: *mut ITEMIDLIST = std::ptr::null_mut();
        let parent: IShellFolder = unsafe { SHBindToParent(first.as_ptr().cast(), Some(&mut last))? };
        let children: Vec<*const ITEMIDLIST> = pidls.iter().map(|p| unsafe { ILFindLastID(p.as_ptr().cast()) as *const ITEMIDLIST }).collect();
        let menu: IContextMenu = unsafe { ui_object(&parent, owner, &children)? };
        Self::new(menu)
    }

    /// Background menu of a folder ("新建", "粘贴", "属性" …).
    pub fn for_folder_background(owner: HWND, folder: &IShellFolder) -> Result<Self> {
        let menu: IContextMenu = unsafe { folder.CreateViewObject(owner)? };
        Self::new(menu)
    }

    pub fn for_view(view: &IShellView, selection: bool) -> Result<Self> {
        let menu: IContextMenu = unsafe { view.GetItemObject(if selection { SVGIO_SELECTION } else { SVGIO_BACKGROUND })? };
        Self::new(menu)
    }

    pub fn hmenu(&self) -> HMENU { self.hmenu }

    /// Fills the popup menu. `extended` adds Shift-only verbs.
    pub fn populate(&self, rename: bool, extended: bool) -> Result<()> {
        // Permit handlers that support it to obtain expensive state off the
        // menu's UI path. Windows retains normal state evaluation otherwise.
        let mut flags = CMF_NORMAL | CMF_EXPLORE | CMF_ASYNCVERBSTATE;
        if rename { flags |= CMF_CANRENAME; }
        if extended { flags |= CMF_EXTENDEDVERBS; }
        unsafe { self.menu.QueryContextMenu(self.hmenu, 0, SHELL_FIRST, SHELL_LAST, flags) }.ok()
    }

    /// Direct canonical commands need no third-party visual menu construction.
    /// Shell handlers receive the documented optimize-for-invoke hint.
    pub fn populate_for_invoke(&self, extended: bool) -> Result<()> {
        let flags = CMF_NORMAL | CMF_EXPLORE | CMF_OPTIMIZEFORINVOKE
            | if extended { CMF_EXTENDEDVERBS } else { CMF_NORMAL };
        unsafe { self.menu.QueryContextMenu(self.hmenu, 0, SHELL_FIRST, SHELL_LAST, flags) }.ok()
    }

    /// Shows `menu` (this menu or one of its submenus) and returns the chosen id.
    pub fn track(&self, owner: HWND, menu: HMENU, point: POINT) -> Option<u32> {
        self.track_with_policy(owner, menu, point, true)
    }

    /// Explicit command-bar menus retain their available commands independently
    /// of which commands are shown by a file's context menu.
    pub fn track_unfiltered(&self, owner: HWND, menu: HMENU, point: POINT) -> Option<u32> {
        self.track_with_policy(owner, menu, point, false)
    }

    fn track_with_policy(&self, owner: HWND, menu: HMENU, point: POINT, context: bool) -> Option<u32> {
        let options = context_menu_options();
        // This path always hosts an original Shell HMENU, including desktop
        // fences while Explorer uses the Windows 11 presentation.
        let filter = context;
        if filter { self.filter(menu, &options); }
        let _active = ActiveMenuScope::enter(&self.menu, filter);
        let flags = TRACK_POPUP_MENU_FLAGS(TPM_RETURNCMD.0 | TPM_RIGHTBUTTON.0 | TPM_LEFTALIGN.0 | TPM_TOPALIGN.0);
        let chosen = unsafe { TrackPopupMenuEx(menu, flags.0, point.x, point.y, owner, None) }.0 as u32;
        (chosen != 0).then_some(chosen)
    }

    /// Removes entries by canonical verb, retaining native IDs and handlers.
    /// Text matching is used only for named extension groups such as 7-Zip.
    pub fn filter(&self, menu: HMENU, options: &ContextMenuOptions) {
        filter_native_menu(menu, &self.menu, options);
    }

    pub fn is_shell_id(id: u32) -> bool { (SHELL_FIRST..=SHELL_LAST).contains(&id) }

    /// Canonical verb ("open", "rename", "delete", "NewFolder" …) for a Shell id.
    pub fn verb(&self, id: u32) -> Option<String> {
        canonical_verb(&self.menu, id)
    }

    /// Finds the submenu that contains `verb` (used to extract "新建").
    pub fn submenu_with_verb(&self, verb: &str) -> Option<HMENU> {
        let count = unsafe { GetMenuItemCount(Some(self.hmenu)) };
        for index in 0..count.max(0) {
            let sub = unsafe { GetSubMenu(self.hmenu, index) };
            if sub.is_invalid() { continue; }
            // "新建" and similar handlers fill their submenu on WM_INITMENUPOPUP.
            self.init_popup(sub, index);
            let inner = unsafe { GetMenuItemCount(Some(sub)) };
            for i in 0..inner.max(0) {
                let id = unsafe { GetMenuItemID(sub, i) };
                if self.verb(id).is_some_and(|v| v.eq_ignore_ascii_case(verb)) { return Some(sub); }
            }
        }
        None
    }

    fn init_popup(&self, sub: HMENU, index: i32) {
        let (wp, lp) = (WPARAM(sub.0 as usize), LPARAM(index as isize));
        if let Ok(menu3) = self.menu.cast::<IContextMenu3>() {
            let mut result = LRESULT(0);
            let _ = unsafe { menu3.HandleMenuMsg2(WM_INITMENUPOPUP, wp, lp, Some(&mut result)) };
        } else if let Ok(menu2) = self.menu.cast::<IContextMenu2>() {
            let _ = unsafe { menu2.HandleMenuMsg(WM_INITMENUPOPUP, wp, lp) };
        }
    }

    /// Runs a Shell command chosen from the menu.
    pub fn invoke(&self, owner: HWND, id: u32, point: Option<POINT>, shift: bool, control: bool) -> Result<()> {
        let offset = (id - SHELL_FIRST) as usize;
        self.invoke_raw(owner, PCSTR(offset as *const u8), PCWSTR(offset as *const u16), point, shift, control)
    }

    /// Runs a canonical verb without showing the menu.
    pub fn invoke_verb(&self, owner: HWND, verb: &str, shift: bool) -> Result<()> {
        let ansi: Vec<u8> = verb.bytes().chain(Some(0)).collect();
        let wide: Vec<u16> = verb.encode_utf16().chain(Some(0)).collect();
        self.invoke_raw(owner, PCSTR(ansi.as_ptr()), PCWSTR(wide.as_ptr()), None, shift, false)
    }

    fn invoke_raw(&self, owner: HWND, verb: PCSTR, verb_w: PCWSTR, point: Option<POINT>, shift: bool, control: bool) -> Result<()> {
        let mut mask = CMIC_MASK_UNICODE;
        if point.is_some() { mask |= CMIC_MASK_PTINVOKE; }
        if shift { mask |= CMIC_MASK_SHIFT_DOWN; }
        if control { mask |= CMIC_MASK_CONTROL_DOWN; }
        let info = CMINVOKECOMMANDINFOEX {
            cbSize: std::mem::size_of::<CMINVOKECOMMANDINFOEX>() as u32,
            fMask: mask, hwnd: owner, lpVerb: verb, nShow: SW_SHOWNORMAL.0, lpVerbW: verb_w,
            ptInvoke: point.unwrap_or_default(), ..Default::default()
        };
        unsafe { self.menu.InvokeCommand((&info as *const CMINVOKECOMMANDINFOEX).cast::<CMINVOKECOMMANDINFO>()) }
    }
}

fn should_hide(verb: Option<&str>, label: &str, options: &ContextMenuOptions) -> bool {
    let canonical = verb.unwrap_or_default().to_ascii_lowercase();
    if options.hidden_verbs.iter().any(|v| v == &canonical) { return true; }
    if command_feature(&canonical) & modern_hidden_features(options) != 0 { return true; }
    let label = label.replace('&', "").to_lowercase();
    let group = label.split('(').next().unwrap_or(&label).trim();
    if options.hidden_verbs.iter().any(|v| v == "new") && matches!(group, "新建" | "新增" | "new") { return true; }
    (!options.show_seven_zip && (label.contains("7-zip") || label.contains("7zip") || canonical.starts_with("7-zip")))
        || (!options.show_wps && (label.contains("wps") || canonical.starts_with("wps")))
}

fn canonical_verb(context: &IContextMenu, id: u32) -> Option<String> {
    if !ShellMenu::is_shell_id(id) { return None; }
    let mut buffer = [0u16; 128];
    unsafe { context.GetCommandString((id - SHELL_FIRST) as usize, GCS_VERBW, None, PSTR(buffer.as_mut_ptr().cast()), buffer.len() as u32) }.ok()?;
    let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    Some(String::from_utf16_lossy(&buffer[..end]))
}

fn filter_native_menu(menu: HMENU, context: &IContextMenu, options: &ContextMenuOptions) {
    let count = unsafe { GetMenuItemCount(Some(menu)) }.max(0);
    for index in (0..count).rev() {
        let id = unsafe { GetMenuItemID(menu, index) };
        if id >= APP_FIRST && id != u32::MAX { continue; }
        let mut text = [0u16; 1024];
        let length = unsafe { GetMenuStringW(menu, index as u32, Some(&mut text), MF_BYPOSITION) }.max(0) as usize;
        let label = String::from_utf16_lossy(&text[..length]);
        // With only named extension filtering enabled there is no need to call
        // every third-party handler's GetCommandString on each popup.
        let verb = if options.hidden_verbs.is_empty() { None } else { canonical_verb(context, id) };
        if should_hide(verb.as_deref(), &label, options) {
            let _ = unsafe { DeleteMenu(menu, index as u32, MF_BYPOSITION) };
        } else {
            let child = unsafe { GetSubMenu(menu, index) };
            if !child.is_invalid() { filter_native_menu(child, context, options); }
        }
    }
    trim_separators(menu);
}

fn trim_separators(menu: HMENU) {
    let mut previous_separator = true;
    let mut index = 0;
    loop {
        let count = unsafe { GetMenuItemCount(Some(menu)) }.max(0);
        if index >= count { break; }
        let flags = unsafe { GetMenuState(menu, index as u32, MF_BYPOSITION) };
        let separator = flags & MF_SEPARATOR.0 != 0;
        if separator && (previous_separator || index + 1 == count) {
            let _ = unsafe { DeleteMenu(menu, index as u32, MF_BYPOSITION) };
        } else { previous_separator = separator; index += 1; }
    }
}

const VIEW_MENU_SUBCLASS: usize = 0x5A53_4D50;
struct ViewMenuState { view: IShellView, modern: Rc<Cell<bool>> }

/// Hosts the original menu only in original mode, using the view's own native
/// IContextMenu. This lets filters keep a precise canonical-verb/command map.
pub(crate) unsafe fn attach_view_menu(view: &IShellView, modern: Rc<Cell<bool>>) {
    let Ok(window) = view.cast::<IOleWindow>().and_then(|w| unsafe { w.GetWindow() }) else { return };
    let mut existing = 0usize;
    if unsafe { GetWindowSubclass(window, Some(view_menu_proc), VIEW_MENU_SUBCLASS, Some(&mut existing)) }.as_bool() {
        if existing != 0 { unsafe { (*(existing as *mut ViewMenuState)).modern = modern; } }
        return;
    }
    let state = Box::into_raw(Box::new(ViewMenuState { view: view.clone(), modern }));
    if !unsafe { SetWindowSubclass(window, Some(view_menu_proc), VIEW_MENU_SUBCLASS, state as usize) }.as_bool() {
        unsafe { drop(Box::from_raw(state)); }
    }
}

unsafe extern "system" fn view_menu_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM, _: usize, data: usize) -> LRESULT {
    if msg == WM_NCDESTROY {
        unsafe { let _ = RemoveWindowSubclass(hwnd, Some(view_menu_proc), VIEW_MENU_SUBCLASS); }
        let state = unsafe { Box::from_raw(data as *mut ViewMenuState) };
        let result = unsafe { DefSubclassProc(hwnd, msg, wp, lp) }; drop(state); return result;
    }
    if let Some(result) = forward_menu_message(msg, wp, lp) { return result; }
    let state = unsafe { &*(data as *const ViewMenuState) };
    if msg == WM_CONTEXTMENU && !state.modern.get() {
        let view = state.view.clone();
        if show_original_view_menu(hwnd, &view, lp).is_ok() { return LRESULT(0); }
    }
    unsafe { DefSubclassProc(hwnd, msg, wp, lp) }
}

fn show_original_view_menu(owner: HWND, view: &IShellView, position: LPARAM) -> Result<()> {
    let point = POINT { x: (position.0 as u32 & 0xffff) as i16 as i32, y: ((position.0 as u32 >> 16) & 0xffff) as i16 as i32 };
    show_original_view_menu_at(owner, view, point)
}

#[cfg(all(target_arch = "x86_64", target_env = "msvc"))]
unsafe extern "system" fn show_expanded_view_menu(raw_view: *mut core::ffi::c_void, point: POINT) -> windows::core::HRESULT {
    let Some(view) = (unsafe { IShellView::from_raw_borrowed(&raw_view) }) else {
        return windows::core::HRESULT(0x8000_4003u32 as i32);
    };
    let result = view.cast::<IOleWindow>().and_then(|window| unsafe { window.GetWindow() })
        .and_then(|owner| show_original_view_menu_at(owner, view, point));
    match result { Ok(()) => windows::core::HRESULT(0), Err(error) => error.code() }
}

fn show_original_view_menu_at(owner: HWND, view: &IShellView, mut point: POINT) -> Result<()> {
    let selected = view.cast::<IFolderView2>().ok()
        .and_then(|folder| unsafe { folder.ItemCount(SVGIO_SELECTION) }.ok()).unwrap_or(0) > 0;
    let menu = ShellMenu::for_view(view, selected)?;
    let shift = unsafe { GetKeyState(VK_SHIFT.0 as i32) } < 0;
    let control = unsafe { GetKeyState(VK_CONTROL.0 as i32) } < 0;
    menu.populate(true, shift)?;
    if point.x == -1 && point.y == -1 {
        let mut rect = windows::Win32::Foundation::RECT::default();
        if unsafe { GetWindowRect(owner, &mut rect) }.is_ok() { point = POINT { x: rect.left + 24, y: rect.top + 24 }; }
        else { let _ = unsafe { GetCursorPos(&mut point) }; }
    }
    if let Some(id) = menu.track(owner, menu.hmenu(), point) {
        // The menu interaction was handled even if the selected Shell command
        // fails; falling through would open a second context menu.
        let _ = menu.invoke(owner, id, Some(point), shift, control);
    }
    Ok(())
}

#[cfg(test)]
mod policy_tests {
    use super::*;
    #[test]
    fn modern_policy_covers_visible_functions_and_keeps_extensions_independent() {
        let mut options = ContextMenuOptions::default();
        assert_eq!(modern_hidden_features(&options), 1 << 11);
        options.hidden_verbs = ["cut", "copy", "paste", "pastelink", "copyaspath",
            "rename", "delete", "properties", "windows.modernshare", "new", "custom-third-party"]
            .map(str::to_string).to_vec();
        options.show_seven_zip = false;
        assert_eq!(modern_hidden_features(&options), 0x0fff);
        options.hidden_verbs.clear();
        options.show_seven_zip = true;
        options.show_wps = true;
        assert_eq!(modern_hidden_features(&options), 0);
    }
    #[test]
    fn filters_canonical_commands_without_using_menu_positions() {
        let options = ContextMenuOptions { modern:false, hidden_verbs:vec!["rename".into(),"delete".into()], ..Default::default() };
        assert!(should_hide(Some("ReName"), "任意本地化名称", &options));
        assert!(!should_hide(Some("open"), "重命名相关文档", &options));
        assert!(!should_hide(None, "打开", &options));
    }
    #[test]
    fn expanded_original_menu_uses_the_same_semantic_aliases() {
        let options = ContextMenuOptions { hidden_verbs: vec!["share".into(), "new".into(), "copyaspath".into()], ..Default::default() };
        for alias in ["Windows.Share", "Windows.ModernShareFlyout", "NewFolder", "Windows.CopyAsPath"] {
            assert!(should_hide(Some(alias), "任意显示名称", &options));
        }
        assert!(!should_hide(Some("open"), "共享文件", &options));
    }
    #[test]
    fn extension_groups_are_independent_and_restorable() {
        let mut options = ContextMenuOptions { show_seven_zip:false, ..Default::default() };
        assert!(should_hide(None,"&7-Zip",&options));
        assert!(should_hide(None,"WPS 文件压缩",&options));
        assert!(!should_hide(None,"压缩为 ZIP 文件",&options));
        options.show_seven_zip=true;options.show_wps=true;
        assert!(!should_hide(None,"7-Zip",&options));
        assert!(!should_hide(None,"WPS 文件压缩",&options));
    }
}

impl Drop for ShellMenu {
    fn drop(&mut self) { let _ = unsafe { DestroyMenu(self.hmenu) }; }
}

unsafe fn ui_object<T: Interface>(parent: &IShellFolder, owner: HWND, children: &[*const ITEMIDLIST]) -> Result<T> {
    unsafe { parent.GetUIObjectOf::<T>(owner, children, None) }
}

/// Returns an interface implemented for a set of sibling items (IDataObject, IDropTarget …).
pub fn items_ui_object<T: Interface>(owner: HWND, pidls: &[Vec<u8>]) -> Result<T> {
    let first = pidls.first().ok_or_else(|| windows::core::Error::from_hresult(windows::core::HRESULT(0x80070057u32 as i32)))?;
    let parent: IShellFolder = unsafe { SHBindToParent(first.as_ptr().cast(), None)? };
    let children: Vec<*const ITEMIDLIST> = pidls.iter().map(|p| unsafe { ILFindLastID(p.as_ptr().cast()) as *const ITEMIDLIST }).collect();
    unsafe { ui_object(&parent, owner, &children) }
}

/// Parent PIDL bytes of an absolute PIDL (empty list for desktop children).
pub fn parent_pidl(pidl: &[u8]) -> Vec<u8> {
    let copy = unsafe { ILClone(pidl.as_ptr().cast()) };
    if copy.is_null() { return vec![0, 0]; }
    unsafe { let _ = ILRemoveLastID(Some(copy)); }
    let bytes = super::pidl::pidl_bytes(copy);
    unsafe { CoTaskMemFree(Some(copy.cast())) };
    bytes
}
