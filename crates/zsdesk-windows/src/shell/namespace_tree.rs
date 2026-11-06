//! Navigation tree (`INameSpaceTreeControl`) styled like the Windows 11 pane.
//!
//! The system Quick access namespace provides the user's pinned/frequent
//! folders. Known folders remain available on systems without that namespace.
//! Roots use the Shell's group padding and DPI-scaled, comfortably spaced rows.

use std::cell::RefCell;

use windows::{
    core::{implement, Error, Interface, Ref, Result, BOOL, HRESULT, PCWSTR, PWSTR},
    Win32::{
        Foundation::{COLORREF, HWND, LPARAM, RECT, WPARAM},
        Graphics::Gdi::{GetObjectW, HGDIOBJ, LOGFONTW},
        Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES,
        System::{
            Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_INPROC_SERVER},
            Ole::IOleWindow,
            SystemServices::{SFGAO_FOLDER, SFGAO_STREAM},
        },
        UI::{
            Controls::{TVM_SETBKCOLOR, TVM_SETTEXTCOLOR, TVM_SETEXTENDEDSTYLE, TVM_SETINDENT,
                TVM_SETITEMHEIGHT, TVS_EX_DOUBLEBUFFER},
            HiDpi::GetDpiForWindow,
            Shell::{
                IContextMenu, INameSpaceTreeControl, INameSpaceTreeControl2, INameSpaceTreeControlEvents,
                INameSpaceTreeControlEvents_Impl, IShellItem, IShellItemArray, IShellItemFilter, IShellItemFilter_Impl,
                NamespaceTreeControl, SHGetFileInfoW, SHGetIDListFromObject, SHGetKnownFolderItem, NSTCECT_BUTTON,
                NSTCECT_LBUTTON, NSTCECT_MBUTTON, NSTCEHT_ONITEM, NSTCEHT_ONITEMBUTTON, NSTCEHT_ONITEMINDENT,
                NSTCEHT_ONITEMRIGHT, NSTCIS_EXPANDED, NSTCIS_SELECTED, NSTCIS_SELECTEDNOEXPAND, NSTCRS_EXPANDED, NSTCRS_VISIBLE,
                SICHINT_CANONICAL,
                NSTCS_AUTOHSCROLL, NSTCS_FULLROWSELECT, NSTCS_HASEXPANDOS, NSTCS_NOEDITLABELS,
                NSTCS_SHOWSELECTIONALWAYS, NSTCS_FADEINOUTEXPANDOS, NSTCS_TABSTOP, NSTCS_ROOTHASEXPANDO,
                NSTCS_ALLOWJUNCTIONS, NSTCS2_DISPLAYPADDING, NSTCS2_INTERRUPTNOTIFICATIONS,
                NTSCS2_NOSINGLETONAUTOEXPAND,
                SHCONTF_FOLDERS, SHFILEINFOW, SHGFI_OPENICON, SHGFI_PIDL, SHGFI_SMALLICON, SHGFI_SYSICONINDEX,
                FOLDERID_Desktop, FOLDERID_Downloads, FOLDERID_Documents, FOLDERID_Pictures,
                FOLDERID_Music, FOLDERID_Videos, FOLDERID_RecycleBinFolder, FOLDERID_NetworkFolder,
                KF_FLAG_DEFAULT,
            },
            WindowsAndMessaging::{EnumChildWindows, FindWindowExW, GetClassNameW, GetClientRect, MoveWindow,
                PostMessageW, ShowWindow, SW_HIDE, SW_SHOW, SendMessageW, WM_GETFONT},
        },
    },
};
use zsdesk_core::model::explorer::ShellLocation;

use super::{
    events::WM_TREE_NAVIGATE,
    shell_item::{location_from_shell_item, shell_item},
};

const S_FALSE: HRESULT = HRESULT(1);

#[cfg(test)]
thread_local! {
    static CLICK_TRACE: RefCell<Vec<(u32, u32, u32)>> = const { RefCell::new(Vec::new()) };
}

#[cfg(test)]
pub fn take_click_trace_for_test() -> Vec<(u32, u32, u32)> {
    CLICK_TRACE.with(|trace| std::mem::take(&mut *trace.borrow_mut()))
}

pub struct NamespaceTreeHost {
    tree: INameSpaceTreeControl,
    events: INameSpaceTreeControlEvents,
    _filter: IShellItemFilter,
    advise_cookie: u32,
    hwnd: HWND,
    last_location: RefCell<Option<IShellItem>>,
}

impl NamespaceTreeHost {
    /// Creates the navigation tree.
    ///
    /// # Safety
    ///
    /// COM must be initialized as STA on the current UI thread. `parent` must
    /// be a valid window owned by that thread and outlive this host.
    pub unsafe fn create(parent: HWND, rect: RECT) -> Result<Self> {
        let tree: INameSpaceTreeControl = unsafe { CoCreateInstance(&NamespaceTreeControl, None, CLSCTX_INPROC_SERVER)? };
        let style = (NSTCS_HASEXPANDOS.0 | NSTCS_ROOTHASEXPANDO.0 | NSTCS_FULLROWSELECT.0
            | NSTCS_SHOWSELECTIONALWAYS.0 | NSTCS_AUTOHSCROLL.0 | NSTCS_ALLOWJUNCTIONS.0
            | NSTCS_NOEDITLABELS.0 | NSTCS_FADEINOUTEXPANDOS.0 | NSTCS_TABSTOP.0) as u32;
        let bounds = navigation_rect(rect, unsafe { GetDpiForWindow(parent) });
        unsafe { tree.Initialize(parent, &bounds, style)? };
        let hwnd = unsafe { tree.cast::<IOleWindow>().and_then(|w| w.GetWindow()).unwrap_or_default() };

        if let Ok(extended) = tree.cast::<INameSpaceTreeControl2>() {
            let styles = NSTCS2_DISPLAYPADDING | NSTCS2_INTERRUPTNOTIFICATIONS | NTSCS2_NOSINGLETONAUTOEXPAND;
            let _ = unsafe { extended.SetControlStyle2(styles, styles) };
        }

        let filter: IShellItemFilter = FolderFilter.into();
        let enum_flags = SHCONTF_FOLDERS.0 as u32;

        let quick_access = ShellLocation::Shell {
            parsing_name: "shell:::{679F85CB-0220-4080-B29B-5540CC05AAB6}".into(),
            title: "快速访问".into(),
        };
        let quick_access_added = unsafe { shell_item(&quick_access) }.is_ok_and(|item| {
            unsafe { tree.AppendRoot(&item, enum_flags, (NSTCRS_VISIBLE.0 | NSTCRS_EXPANDED.0) as u32, &filter) }.is_ok()
        });
        if !quick_access_added {
            for folder in [&FOLDERID_Desktop, &FOLDERID_Downloads, &FOLDERID_Documents,
                &FOLDERID_Pictures, &FOLDERID_Music, &FOLDERID_Videos] {
                if let Ok(item) = unsafe { SHGetKnownFolderItem::<IShellItem>(folder, KF_FLAG_DEFAULT, None) } {
                    let _ = unsafe { tree.AppendRoot(&item, enum_flags, NSTCRS_VISIBLE.0 as u32, &filter) };
                }
            }
        }
        let this_pc = unsafe { shell_item(&ShellLocation::ThisPc)? };
        unsafe { tree.AppendRoot(&this_pc, enum_flags, NSTCRS_VISIBLE.0 as u32, &filter)?; }
        for folder in [&FOLDERID_RecycleBinFolder, &FOLDERID_NetworkFolder] {
            if let Ok(item) = unsafe { SHGetKnownFolderItem::<IShellItem>(folder, KF_FLAG_DEFAULT, None) } {
                let _ = unsafe { tree.AppendRoot(&item, enum_flags, NSTCRS_VISIBLE.0 as u32, &filter) };
            }
        }

        let events: INameSpaceTreeControlEvents = TreeEvents { hwnd: parent.0 as isize }.into();
        let advise_cookie = unsafe { tree.TreeAdvise(&events)? };
        let host = Self { tree, events, _filter: filter, advise_cookie, hwnd, last_location: RefCell::new(None) };
        if let Some(view) = host.tree_view() {
            unsafe { SendMessageW(view, TVM_SETEXTENDEDSTYLE, Some(WPARAM(TVS_EX_DOUBLEBUFFER as usize)), Some(LPARAM(TVS_EX_DOUBLEBUFFER as isize))); }
        }
        host.apply_metrics();
        Ok(host)
    }

    fn tree_view(&self) -> Option<HWND> {
        if self.hwnd.0.is_null() { return None; }
        if is_tree_view(self.hwnd) { return Some(self.hwnd); }
        if let Ok(view) = unsafe { FindWindowExW(Some(self.hwnd), None, windows::core::w!("SysTreeView32"), PCWSTR::null()) } {
            return Some(view);
        }
        // Shell versions can introduce an intermediate host window.
        let mut found = HWND::default();
        let _ = unsafe { EnumChildWindows(Some(self.hwnd), Some(find_tree_view), LPARAM((&mut found as *mut HWND) as isize)) };
        (!found.0.is_null()).then_some(found)
    }

    pub fn hwnd(&self) -> HWND { self.hwnd }

    fn apply_metrics(&self) {
        let Some(view) = self.tree_view() else { return };
        let dpi = unsafe { GetDpiForWindow(view) }.max(96);
        let mut font = LOGFONTW::default();
        let handle = unsafe { SendMessageW(view, WM_GETFONT, None, None) };
        if handle.0 != 0 {
            unsafe { GetObjectW(HGDIOBJ(handle.0 as *mut _), std::mem::size_of::<LOGFONTW>() as i32,
                Some((&mut font as *mut LOGFONTW).cast())); }
        }
        // Keep system text-size accessibility settings from clipping a row.
        // Native tree views round odd heights down; round up explicitly.
        let height = (scale(32, dpi).max(font.lfHeight.saturating_abs() + scale(12, dpi)) + 1) & !1;
        unsafe {
            SendMessageW(view, TVM_SETITEMHEIGHT, Some(WPARAM(height as usize)), None);
            SendMessageW(view, TVM_SETINDENT, Some(WPARAM(scale(20, dpi) as usize)), None);
        }
    }

    /// Applies light/dark colors to the tree.
    pub fn apply_theme(&self, dark: bool, background: u32, text: u32) {
        crate::theme::theme_control(self.hwnd, dark);
        if let Some(view) = self.tree_view() {
            crate::theme::theme_control(view, dark);
            unsafe {
                SendMessageW(view, TVM_SETBKCOLOR, None, Some(LPARAM(COLORREF(background).0 as isize)));
                SendMessageW(view, TVM_SETTEXTCOLOR, None, Some(LPARAM(COLORREF(text).0 as isize)));
            }
        }
        self.apply_metrics();
    }

    /// Resizes the native namespace tree.
    ///
    /// # Safety
    ///
    /// Must be called on the STA/UI thread that created this host.
    pub unsafe fn resize(&self, rect: RECT) {
        if self.hwnd.0.is_null() { return; }
        let rect = navigation_rect(rect, unsafe { GetDpiForWindow(self.hwnd) });
        let _ = unsafe { MoveWindow(self.hwnd, rect.left, rect.top, rect.right - rect.left, rect.bottom - rect.top, true) };
        self.apply_metrics();
    }

    pub fn set_visible(&self, visible: bool) {
        if !self.hwnd.0.is_null() { let _ = unsafe { ShowWindow(self.hwnd, if visible { SW_SHOW } else { SW_HIDE }) }; }
    }

    /// Expands the tree down to `location` and selects it.
    ///
    /// # Safety
    ///
    /// Must be called on the STA/UI thread that created this host while its COM
    /// apartment remains initialized.
    pub unsafe fn select_location(&self, location: &ShellLocation) {
        let Ok(item) = (unsafe { shell_item(location) }) else { return };
        let previous = self.last_location.borrow().clone();
        if previous.as_ref().is_some_and(|previous| unsafe { previous.Compare(&item, SICHINT_CANONICAL.0 as u32) }.is_ok_and(|order| order == 0)) {
            return;
        }
        // Prefer an existing Quick access entry instead of expanding the same
        // folder again beneath its drive and pushing quick links off-screen.
        if unsafe { self.tree.SetItemState(&item, NSTCIS_SELECTEDNOEXPAND.0 as u32, NSTCIS_SELECTEDNOEXPAND.0 as u32) }.is_ok()
            && unsafe { self.tree.GetItemState(&item, NSTCIS_SELECTED.0 as u32) }.is_ok_and(|state| state & NSTCIS_SELECTED.0 as u32 != 0) {
            unsafe { self.reveal_selection(&item); }
            *self.last_location.borrow_mut() = Some(item);
            return;
        }
        let mut chain = Vec::new();
        let mut cursor = unsafe { item.GetParent() };
        while let Ok(parent) = cursor {
            cursor = unsafe { parent.GetParent() };
            chain.push(parent);
            if chain.len() > 64 { break; }
        }
        unsafe {
            for ancestor in chain.iter().rev() {
                let _ = self.tree.SetItemState(ancestor, NSTCIS_EXPANDED.0 as u32, NSTCIS_EXPANDED.0 as u32);
            }
            if self.tree.SetItemState(&item, NSTCIS_SELECTEDNOEXPAND.0 as u32, NSTCIS_SELECTEDNOEXPAND.0 as u32).is_ok()
                && self.tree.GetItemState(&item, NSTCIS_SELECTED.0 as u32).is_ok_and(|state| state & NSTCIS_SELECTED.0 as u32 != 0) {
                self.reveal_selection(&item);
                *self.last_location.borrow_mut() = Some(item);
            }
        }
    }

    unsafe fn reveal_selection(&self, item: &IShellItem) {
        let mut client = RECT::default();
        let _ = unsafe { GetClientRect(self.hwnd, &mut client) };
        let visible = unsafe { self.tree.GetItemRect(item) }.is_ok_and(|rect|
            rect.bottom > rect.top && rect.top >= 0 && rect.bottom <= client.bottom);
        if !visible { let _ = unsafe { self.tree.EnsureItemVisible(item) }; }
    }

    /// Disconnects the namespace tree event sink.
    ///
    /// # Safety
    ///
    /// Must be called exactly once on the STA/UI thread that created this host,
    /// before the COM apartment is uninitialized.
    pub unsafe fn destroy(&self) {
        let _keep_events_alive = &self.events;
        let _ = unsafe { self.tree.TreeUnadvise(self.advise_cookie) };
    }
}

fn scale(value: i32, dpi: u32) -> i32 {
    ((i64::from(value) * i64::from(dpi.max(96)) + 48) / 96) as i32
}

fn navigation_rect(rect: RECT, dpi: u32) -> RECT {
    let horizontal = scale(4, dpi).min((rect.right - rect.left).max(0) / 2);
    let vertical = scale(8, dpi).min((rect.bottom - rect.top).max(0) / 2);
    RECT { left: rect.left + horizontal, top: rect.top + vertical,
        right: (rect.right - horizontal).max(rect.left + horizontal),
        bottom: (rect.bottom - vertical).max(rect.top + vertical) }
}

fn is_tree_view(hwnd: HWND) -> bool {
    let mut name = [0u16; 32];
    let length = unsafe { GetClassNameW(hwnd, &mut name) }.max(0) as usize;
    String::from_utf16_lossy(&name[..length]) == "SysTreeView32"
}

unsafe extern "system" fn find_tree_view(hwnd: HWND, result: LPARAM) -> BOOL {
    if is_tree_view(hwnd) {
        unsafe { *(result.0 as *mut HWND) = hwnd; }
        BOOL(0)
    } else { BOOL(1) }
}

impl Drop for NamespaceTreeHost {
    fn drop(&mut self) { unsafe { self.destroy(); } }
}

/// Shows folders only; compressed files stay out of the tree.
#[implement(IShellItemFilter)]
struct FolderFilter;

#[allow(non_snake_case)]
impl IShellItemFilter_Impl for FolderFilter_Impl {
    fn IncludeItem(&self, item: Ref<IShellItem>) -> Result<()> {
        let Some(item) = item.as_ref() else { return Err(Error::from_hresult(S_FALSE)) };
        // Hidden folders follow the enumeration flags; only compressed files are removed.
        let attrs = unsafe { item.GetAttributes(SFGAO_FOLDER | SFGAO_STREAM) }.map(|a| a.0).unwrap_or(0);
        if attrs & SFGAO_FOLDER.0 != 0 && attrs & SFGAO_STREAM.0 == 0 { Ok(()) } else { Err(Error::from_hresult(S_FALSE)) }
    }
    fn GetEnumFlagsForItem(&self, _item: Ref<IShellItem>) -> Result<u32> { Ok(SHCONTF_FOLDERS.0 as u32) }
}

#[implement(INameSpaceTreeControlEvents)]
struct TreeEvents {
    hwnd: isize,
}

impl TreeEvents {
    fn post_item(&self, item: &IShellItem, new_tab: bool) {
        let Some(location) = (unsafe { location_from_shell_item(item) }) else { return };
        let location = Box::into_raw(Box::new(location));
        if unsafe { PostMessageW(Some(HWND(self.hwnd as *mut _)), WM_TREE_NAVIGATE, WPARAM(new_tab as usize), LPARAM(location as isize)) }.is_err() {
            unsafe { drop(Box::from_raw(location)); }
        }
    }
}

/// Returns whether an item click opens a new tab. Structural tree regions stay
/// under native TreeView control and must never navigate the folder view.
fn navigation_click(hit_test: u32, click_type: u32) -> Option<bool> {
    let structural = (NSTCEHT_ONITEMBUTTON.0 | NSTCEHT_ONITEMINDENT.0) as u32;
    // FULLROWSELECT includes the blank area after the label. ONITEM alone only
    // contains the icon, label and state icon, so it misses that part of a row.
    let item_row = (NSTCEHT_ONITEM.0 | NSTCEHT_ONITEMRIGHT.0) as u32;
    if hit_test & structural != 0 || hit_test & item_row == 0 { return None; }
    match click_type & NSTCECT_BUTTON.0 as u32 {
        button if button == NSTCECT_LBUTTON.0 as u32 => Some(false),
        button if button == NSTCECT_MBUTTON.0 as u32 => Some(true),
        _ => None,
    }
}

unsafe fn system_icon_indices(item: &IShellItem) -> Option<(i32, i32)> {
    let pidl = unsafe { SHGetIDListFromObject(item).ok()? };
    let path = PCWSTR(pidl.cast());
    let flags = SHGFI_PIDL | SHGFI_SYSICONINDEX | SHGFI_SMALLICON;
    let mut default_info = SHFILEINFOW::default();
    let mut open_info = SHFILEINFOW::default();
    let info_size = std::mem::size_of::<SHFILEINFOW>() as u32;
    let default_result = unsafe { SHGetFileInfoW(path, FILE_FLAGS_AND_ATTRIBUTES(0), Some(&mut default_info), info_size, flags) };
    let open_result = unsafe { SHGetFileInfoW(path, FILE_FLAGS_AND_ATTRIBUTES(0), Some(&mut open_info), info_size, flags | SHGFI_OPENICON) };
    unsafe { CoTaskMemFree(Some(pidl.cast())) };
    (default_result != 0 && open_result != 0).then_some((default_info.iIcon, open_info.iIcon))
}

#[allow(non_snake_case)]
impl INameSpaceTreeControlEvents_Impl for TreeEvents_Impl {
    fn OnItemClick(&self, shell_item: Ref<IShellItem>, hit_test: u32, click_type: u32) -> Result<()> {
        #[cfg(test)]
        CLICK_TRACE.with(|trace| trace.borrow_mut().push((hit_test, click_type,
            unsafe { windows::Win32::UI::WindowsAndMessaging::GetMessagePos() })));
        if let Some(new_tab) = navigation_click(hit_test, click_type) {
            if let Some(item) = shell_item.as_ref() { self.post_item(item, new_tab); }
            // Middle click is fully handled here; do not change tree selection.
            if new_tab { return Ok(()); }
        }
        // S_OK consumes TreeView's default processing, including expand/collapse.
        Err(Error::from_hresult(S_FALSE))
    }
    fn OnPropertyItemCommit(&self, _item: Ref<IShellItem>) -> Result<()> { Err(Error::from_hresult(S_FALSE)) }
    fn OnItemStateChanging(&self, _item: Ref<IShellItem>, _mask: u32, _state: u32) -> Result<()> { Ok(()) }
    fn OnItemStateChanged(&self, _item: Ref<IShellItem>, _mask: u32, _state: u32) -> Result<()> { Ok(()) }
    fn OnSelectionChanged(&self, _selection: Ref<IShellItemArray>) -> Result<()> { Ok(()) }
    fn OnKeyboardInput(&self, _message: u32, _wparam: WPARAM, _lparam: LPARAM) -> Result<()> { Err(Error::from_hresult(S_FALSE)) }
    fn OnBeforeExpand(&self, _item: Ref<IShellItem>) -> Result<()> { Ok(()) }
    fn OnAfterExpand(&self, _item: Ref<IShellItem>) -> Result<()> { Ok(()) }
    fn OnBeginLabelEdit(&self, _item: Ref<IShellItem>) -> Result<()> { Ok(()) }
    fn OnEndLabelEdit(&self, _item: Ref<IShellItem>) -> Result<()> { Ok(()) }
    fn OnGetToolTip(&self, _item: Ref<IShellItem>, _tip: PWSTR, _length: i32) -> Result<()> { Err(Error::from_hresult(windows::core::HRESULT(0x80004001u32 as i32))) }
    fn OnBeforeItemDelete(&self, _item: Ref<IShellItem>) -> Result<()> { Ok(()) }
    fn OnItemAdded(&self, _item: Ref<IShellItem>, _is_root: BOOL) -> Result<()> { Ok(()) }
    fn OnItemDeleted(&self, _item: Ref<IShellItem>, _is_root: BOOL) -> Result<()> { Ok(()) }
    fn OnBeforeContextMenu(&self, _item: Ref<IShellItem>, _riid: *const windows::core::GUID, result: *mut *mut core::ffi::c_void) -> Result<()> {
        if !result.is_null() { unsafe { *result = std::ptr::null_mut(); } }
        Err(Error::from_hresult(windows::core::HRESULT(0x80004001u32 as i32)))
    }
    fn OnAfterContextMenu(&self, _item: Ref<IShellItem>, _menu: Ref<IContextMenu>, _riid: *const windows::core::GUID, result: *mut *mut core::ffi::c_void) -> Result<()> {
        if !result.is_null() { unsafe { *result = std::ptr::null_mut(); } }
        Err(Error::from_hresult(windows::core::HRESULT(0x80004001u32 as i32)))
    }
    fn OnBeforeStateImageChange(&self, _item: Ref<IShellItem>) -> Result<()> { Ok(()) }
    fn OnGetDefaultIconIndex(&self, item: Ref<IShellItem>, default_icon: *mut i32, open_icon: *mut i32) -> Result<()> {
        let Some(item) = item.as_ref() else { return Err(Error::from_hresult(S_FALSE)) };
        let Some((default_index, open_index)) = (unsafe { system_icon_indices(item) }) else { return Err(Error::from_hresult(S_FALSE)) };
        if default_icon.is_null() || open_icon.is_null() { return Err(Error::from_hresult(S_FALSE)); }
        unsafe { *default_icon = default_index; *open_icon = open_index; }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::Shell::{NSTCECT_DBLCLICK, NSTCECT_RBUTTON,
        NSTCEHT_NOWHERE, NSTCEHT_ONITEMICON, NSTCEHT_ONITEMLABEL, NSTCEHT_ONITEMSTATEICON};

    #[test]
    fn entire_selected_row_opens_the_folder() {
        for hit in [NSTCEHT_ONITEMICON, NSTCEHT_ONITEMLABEL, NSTCEHT_ONITEMSTATEICON, NSTCEHT_ONITEMRIGHT] {
            assert_eq!(navigation_click(hit.0 as u32, NSTCECT_LBUTTON.0 as u32), Some(false));
            assert_eq!(navigation_click(hit.0 as u32, NSTCECT_MBUTTON.0 as u32), Some(true));
        }
    }

    #[test]
    fn expansion_and_indent_never_navigate() {
        for hit in [NSTCEHT_ONITEMBUTTON.0, NSTCEHT_ONITEMINDENT.0, NSTCEHT_NOWHERE.0,
            NSTCEHT_ONITEMBUTTON.0 | NSTCEHT_ONITEMLABEL.0,
            NSTCEHT_ONITEMINDENT.0 | NSTCEHT_ONITEMRIGHT.0] {
            for button in [NSTCECT_LBUTTON.0, NSTCECT_MBUTTON.0, NSTCECT_RBUTTON.0] {
                assert_eq!(navigation_click(hit as u32, button as u32), None);
            }
        }
    }

    #[test]
    fn double_click_flag_preserves_button_identity() {
        let hit = NSTCEHT_ONITEMRIGHT.0 as u32;
        assert_eq!(navigation_click(hit, (NSTCECT_LBUTTON.0 | NSTCECT_DBLCLICK.0) as u32), Some(false));
        assert_eq!(navigation_click(hit, (NSTCECT_MBUTTON.0 | NSTCECT_DBLCLICK.0) as u32), Some(true));
        assert_eq!(navigation_click(hit, (NSTCECT_RBUTTON.0 | NSTCECT_DBLCLICK.0) as u32), None);
        assert_eq!(navigation_click(hit, NSTCECT_RBUTTON.0 as u32), None);
        assert_eq!(navigation_click(hit, 0), None);
    }
}
