use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::path::PathBuf;

#[cfg(all(target_arch = "x86_64", target_env = "msvc"))]
extern "C" {
    fn zs_menu_query(browser: *mut core::ffi::c_void, service: *const windows::core::GUID,
        iid: *const windows::core::GUID, out: *mut *mut core::ffi::c_void) -> HRESULT;
    fn zs_menu_shutdown();
    fn zs_menu_diagnostics(output: *mut MenuDiagnostics);
}
use windows::{
    core::{implement, Error, IUnknown, IUnknownImpl, Interface, Ref, Result, HRESULT},
    Win32::{
        Foundation::{HWND, LPARAM, RECT, WPARAM, PROPERTYKEY},
        System::{
            Com::{
                CoCreateInstance, IServiceProvider, IServiceProvider_Impl, CLSCTX_INPROC_SERVER,
            },
            Ole::IObjectWithSite,
        },
        UI::{
            Shell::{
                ICommDlgBrowser, ICommDlgBrowser2, ICommDlgBrowser2_Impl, ICommDlgBrowser_Impl,
                IExplorerBrowser, IExplorerBrowserEvents, IExplorerBrowserEvents_Impl, IShellView,
                EBO_ALWAYSNAVIGATE, EBO_NOBORDER, EBO_SHOWFRAMES, EBO_NAVIGATEONCE, EBO_NOPERSISTVIEWSTATE, EBF_NODROPTARGET,
                IFolderView2, IResultsFolder, IColumnManager, FVM_DETAILS,
                IExplorerPaneVisibility, IExplorerPaneVisibility_Impl, EP_NavPane, EP_StatusBar,
                EP_Commands, EP_DetailsPane, EP_PreviewPane, EP_Ribbon,
                EPS_DEFAULT_ON, EPS_DEFAULT_OFF, EPS_FORCE, EPS_DONTCARE,
            },
            WindowsAndMessaging::PostMessageW,
        },
    },
};
use zsdesk_core::model::explorer::ShellLocation;

use super::{
    events::{WM_SHELL_FOLDER_INVOKED, WM_SHELL_NAVIGATED, WM_SHELL_NAVIGATION_FAILED, WM_SHELL_SELECTION_CHANGED},
    shell_item::{
        location_from_pidl, shell_item, selected_shell_item, is_folder, location_from_shell_item,
    },
};

const S_FALSE: HRESULT = HRESULT(1);
const E_NOINTERFACE: HRESULT = HRESULT(0x8000_4002u32 as i32);

/// Optional Shell panes shown next to the file list.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Panes { pub preview: bool, pub details: bool }

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct MenuDiagnostics {
    pub requests: u64,
    pub presenter_creations: u64,
    pub selection_snapshots: u64,
    pub agile_commands: u64,
    pub agile_failures: u64,
    pub last_query_microseconds: u64,
    pub last_prepare_microseconds: u64,
    pub runtime_initializations: u64,
    pub last_failure: i32,
    pub reserved: u32,
    pub modern_items_hidden: u64,
    pub modern_extension_loads_skipped: u64,
    pub modern_updates_skipped: u64,
    pub modern_filter_failures: u64,
}
pub fn menu_diagnostics() -> MenuDiagnostics {
    let mut value = MenuDiagnostics::default();
    #[cfg(all(target_arch = "x86_64", target_env = "msvc"))]
    unsafe { zs_menu_diagnostics(&mut value); }
    value
}

pub struct ExplorerBrowserHost {
    panes: Rc<Cell<Panes>>,
    modern: Rc<Cell<bool>>,
    pub(crate) selection_revision: Rc<Cell<u64>>,
    pub(crate) selection_size: RefCell<super::commands::SelectionSizeCache>,
    browser: Rc<IExplorerBrowser>,
    events: IExplorerBrowserEvents,
    site: IServiceProvider,
    advise_cookie: u32,
    destroyed: Cell<bool>,
    owner: isize,
    streamed_results_used: Cell<bool>,
    results_active: Rc<Cell<bool>>,
    results_location: Rc<RefCell<Option<ShellLocation>>>,
}

impl ExplorerBrowserHost {
    /// Creates a host that does not persist Shell-managed folder view settings.
    ///
    /// # Safety
    /// The same STA and parent-window lifetime requirements as `create` apply.
    pub unsafe fn create_transient(parent: HWND, rect: RECT, modern: bool, panes: Panes) -> Result<Self> {
        let host = unsafe { Self::create(parent, rect, modern, panes)? };
        let options = unsafe { host.browser.GetOptions()? };
        unsafe { host.browser.SetOptions(options | EBO_NOPERSISTVIEWSTATE)?; }
        Ok(host)
    }

    /// Creates and initializes the native Shell browser child window.
    ///
    /// # Safety
    ///
    /// COM must be initialized as STA on the current UI thread. `parent` must
    /// be a valid window owned by that thread and outlive this host.
    pub unsafe fn create(parent: HWND, rect: RECT, modern: bool, panes: Panes) -> Result<Self> {
        let browser: IExplorerBrowser = unsafe {
            CoCreateInstance(
                &windows::Win32::UI::Shell::ExplorerBrowser,
                None,
                CLSCTX_INPROC_SERVER,
            )?
        };

        let mut options = super::menu::context_menu_options(); options.modern = modern;
        super::menu::set_context_menu_options(options);
        let browser = Rc::new(browser);
        let panes = Rc::new(Cell::new(panes));
        let modern = Rc::new(Cell::new(modern));
        let selection_revision = Rc::new(Cell::new(0u64));
        let results_active = Rc::new(Cell::new(false));
        let results_location = Rc::new(RefCell::new(None));
        let site: IServiceProvider = BrowserSite {
            hwnd: parent.0 as isize,
            browser: Rc::downgrade(&browser),
            modern: modern.clone(),
            panes: panes.clone(),
            selection_pending: Rc::new(Cell::new(false)),
            selection_revision: selection_revision.clone(),
        }
        .into();
        let object_with_site: IObjectWithSite = browser.cast()?;
        unsafe { object_with_site.SetSite(&site)? };

        unsafe {
            browser.SetOptions(EBO_SHOWFRAMES | EBO_ALWAYSNAVIGATE | EBO_NOBORDER)?;
            browser.Initialize(parent, &rect, None)?;
        }

        let events: IExplorerBrowserEvents = BrowserEvents {
            hwnd: parent.0 as isize,
            browser: Rc::downgrade(&browser),
            modern: modern.clone(),
            selection_revision: selection_revision.clone(),
            results_active: results_active.clone(),
            results_location: results_location.clone(),
        }
        .into();
        let advise_cookie = unsafe { browser.Advise(&events)? };

        let host = Self {
            panes,
            modern,
            selection_revision,
            selection_size: RefCell::new(Default::default()),
            browser,
            events,
            site,
            advise_cookie,
            destroyed: Cell::new(false),
            owner: parent.0 as isize,
            streamed_results_used: Cell::new(false),
            results_active,
            results_location,
        };
        Ok(host)
    }

    /// Navigates the hosted Shell view without invoking an external Explorer.
    ///
    /// # Safety
    ///
    /// Must be called on the STA/UI thread that created this host while its
    /// parent window and COM apartment are still alive.
    pub unsafe fn navigate(&self, location: &ShellLocation) -> Result<()> {
        if let ShellLocation::Search { query, scope: None } = location {
            if query.trim().is_empty() || query.contains('\0') {
                return Err(Error::from_hresult(HRESULT(0x80070057u32 as i32)));
            }
            *self.results_location.borrow_mut() = Some(location.clone());
            return self.begin_search_results();
        }
        self.results_active.set(false);
        *self.results_location.borrow_mut() = None;
        // FillFromObject adds NAVIGATEONCE internally. Remove it before normal
        // navigation, otherwise BrowseToObject fails after a search results view.
        let options = unsafe { self.browser.GetOptions()? };
        if options.0 & EBO_NAVIGATEONCE.0 != 0 {
            unsafe { self.browser.SetOptions(windows::Win32::UI::Shell::EXPLORER_BROWSER_OPTIONS(options.0 & !EBO_NAVIGATEONCE.0))?; }
        }
        self.selection_revision.set(self.selection_revision.get().wrapping_add(1));
        let item = unsafe { shell_item(location)? };
        unsafe { self.browser.BrowseToObject(&item, 0) }
    }

    /// True after this browser has hosted a streamed ResultsFolder. Windows
    /// does not reliably allow a second FillFromObject after leaving that view;
    /// recreate the host when entering global results from a normal folder.
    pub fn has_streamed_results(&self) -> bool { self.streamed_results_used.get() }

    /// Creates an empty native results view, or clears its current query.
    /// A fresh Fill sends the real Shell navigation completion. RemoveAll does
    /// not, so a reused results view posts exactly one logical completion here.
    /// Prefer navigate(Search{scope:None,..}) to retain the query in events.
    pub fn begin_search_results(&self) -> Result<()> {
        self.selection_revision.set(self.selection_revision.get().wrapping_add(1));
        if self.streamed_results_used.get() {
            if !self.results_active.get() {
                return Err(Error::from_hresult(HRESULT(0x8000FFFFu32 as i32)));
            }
            let results = self.results_folder()?;
            unsafe { results.RemoveAll()?; }
            if let Some(location) = self.results_location.borrow().clone() {
                post_location(self.owner, WM_SHELL_NAVIGATED, location);
            }
            return Ok(());
        }
        self.streamed_results_used.set(true);
        self.results_active.set(true);
        let result = unsafe { self.browser.FillFromObject(None::<&IUnknown>, EBF_NODROPTARGET) };
        if result.is_err() { self.results_active.set(false); }
        result
    }

    fn results_folder(&self) -> Result<IResultsFolder> {
        let view = unsafe { self.current_view()? };
        let folder: IFolderView2 = view.cast()?;
        unsafe { folder.GetFolder() }
    }

    /// Appends one UI-thread batch of at most 64 paths. Disappeared or
    /// inaccessible entries are skipped. The caller consumes the entire batch
    /// and must reject stale query generations before calling this method.
    pub fn append_search_paths(&self, paths: &[PathBuf]) -> Result<usize> {
        if paths.len() > 64 { return Err(Error::from_hresult(HRESULT(0x80070057u32 as i32))); }
        if !self.results_active.get() { return Err(Error::from_hresult(HRESULT(0x8000FFFFu32 as i32))); }
        let results = self.results_folder().map_err(|_| Error::from_hresult(HRESULT(0x8000000Au32 as i32)))?;
        let mut added = 0;
        for path in paths {
            if !path.is_absolute() { continue; }
            let Ok(item) = (unsafe { shell_item(&ShellLocation::Path(path.clone())) }) else { continue };
            if unsafe { results.AddItem(&item) }.is_ok() { added += 1; }
        }
        if added > 0 { self.selection_revision.set(self.selection_revision.get().wrapping_add(1)); }
        Ok(added)
    }

    /// Resizes the hosted Shell view.
    ///
    /// # Safety
    ///
    /// Must be called on the STA/UI thread that created this host.
    pub unsafe fn resize(&self, rect: RECT) {
        let _ = unsafe { self.browser.SetRect(None, rect) };
    }

    /// Disconnects callbacks and destroys the hosted Shell view.
    ///
    /// # Safety
    ///
    /// Must be called exactly once on the STA/UI thread that created this host,
    /// before its parent window or COM apartment is destroyed.
    pub unsafe fn destroy(&self) {
        if self.destroyed.replace(true) { return; }
        // Block reentrant service queries while this particular browser is
        // being torn down; another/new browser may still enable its own mode.
        self.modern.set(false);
        #[cfg(all(target_arch = "x86_64", target_env = "msvc"))]
        unsafe { zs_menu_shutdown(); }
        let _keep_callbacks_alive = (&self.events, &self.site);
        let _ = unsafe { self.browser.Unadvise(self.advise_cookie) };
        if let Ok(object_with_site) = self.browser.cast::<IObjectWithSite>() {
            let _ = unsafe { object_with_site.SetSite(None::<&IUnknown>) };
        }
        let _ = unsafe { self.browser.Destroy() };
    }
}

impl ExplorerBrowserHost {
    pub fn panes(&self) -> Panes { self.panes.get() }
    /// Changes presentation without rebuilding the file view or losing its
    /// selection. The native XAML runtime is retained once per UI thread.
    pub fn set_modern_menu(&self, modern: bool) {
        if self.modern.replace(modern) != modern {
            #[cfg(all(target_arch = "x86_64", target_env = "msvc"))]
            unsafe { zs_menu_shutdown(); }
        }
        let mut options = super::menu::context_menu_options(); options.modern = modern;
        super::menu::set_context_menu_options(options);
    }
    pub fn set_context_menu_options(&self, options: super::menu::ContextMenuOptions) {
        self.set_modern_menu(options.modern);
        super::menu::set_context_menu_options(options);
    }
    /// Changes optional panes; they apply on the next navigation (callers refresh).
    pub fn set_panes(&self, panes: Panes) { self.panes.set(panes); }
}

impl Drop for ExplorerBrowserHost {
    fn drop(&mut self) { unsafe { self.destroy(); } }
}

impl ExplorerBrowserHost {
    pub unsafe fn current_view(&self) -> Result<IShellView> {
        unsafe { self.browser.GetCurrentView() }
    }
    pub unsafe fn translate(&self, message: &windows::Win32::UI::WindowsAndMessaging::MSG) -> bool {
        unsafe { self.current_view().map(|v| v.TranslateAccelerator(message) == HRESULT(0)).unwrap_or(false) }
    }
    pub unsafe fn focus(&self) {
        if let Ok(view) = unsafe { self.current_view() } {
            let _ = unsafe { view.UIActivate(windows::Win32::UI::Shell::SVUIA_ACTIVATE_FOCUS.0 as u32) };
        }
    }
}

#[implement(IServiceProvider, ICommDlgBrowser2, IExplorerPaneVisibility)]
struct BrowserSite {
    hwnd: isize,
    browser: Weak<IExplorerBrowser>,
    modern: Rc<Cell<bool>>,
    panes: Rc<Cell<Panes>>,
    selection_pending: Rc<Cell<bool>>,
    selection_revision: Rc<Cell<u64>>,
}

#[allow(non_snake_case)]
impl IServiceProvider_Impl for BrowserSite_Impl {
    fn QueryService(
        &self,
        guidservice: *const windows::core::GUID,
        riid: *const windows::core::GUID,
        ppvobject: *mut *mut core::ffi::c_void,
    ) -> Result<()> {
        if guidservice.is_null() || riid.is_null() || ppvobject.is_null() {
            return Err(Error::from_hresult(E_NOINTERFACE));
        }
        unsafe { *ppvobject = std::ptr::null_mut(); }
        if unsafe { *guidservice } != ICommDlgBrowser::IID && unsafe { *guidservice } != IExplorerPaneVisibility::IID {
            #[cfg(all(target_arch = "x86_64", target_env = "msvc"))]
            if let Some(browser) = self.browser.upgrade().filter(|_|self.modern.get()) {
                return unsafe { zs_menu_query(browser.as_raw(), guidservice, riid, ppvobject) }.ok();
            }
            return Err(Error::from_hresult(E_NOINTERFACE));
        }
        let result = unsafe { self.QueryInterface(riid, ppvobject) };
        result.ok()
    }
}

#[allow(non_snake_case)]
impl IExplorerPaneVisibility_Impl for BrowserSite_Impl {
    fn GetPaneState(&self, ep: *const windows::core::GUID) -> Result<u32> {
        if ep.is_null() { return Ok(EPS_DONTCARE.0 as u32); }
        let pane = unsafe { *ep };
        let panes = self.panes.get();
        let on = (EPS_DEFAULT_ON.0 | EPS_FORCE.0) as u32;
        let off = (EPS_DEFAULT_OFF.0 | EPS_FORCE.0) as u32;
        // ZSDesk draws its own navigation tree, command bar and status bar.
        if pane == EP_PreviewPane { Ok(if panes.preview { on } else { off }) }
        else if pane == EP_DetailsPane { Ok(if panes.details { on } else { off }) }
        else if [EP_NavPane, EP_StatusBar, EP_Commands, EP_Ribbon].contains(&pane) { Ok(off) }
        else { Ok(EPS_DONTCARE.0 as u32) }
    }
}

#[allow(non_snake_case)]
impl ICommDlgBrowser_Impl for BrowserSite_Impl {
    fn OnDefaultCommand(&self, view:Ref<IShellView>)->Result<()> {
        let Some(view)=view.as_ref() else{return Err(Error::from_hresult(E_NOINTERFACE));};
        let Ok(item)=(unsafe {selected_shell_item(view)}) else{return Err(Error::from_hresult(S_FALSE));};
        if !unsafe {is_folder(&item)} {return Err(Error::from_hresult(S_FALSE));}
        let Some(location)=(unsafe {location_from_shell_item(&item)}) else{return Err(Error::from_hresult(S_FALSE));};
        post_location(self.hwnd,WM_SHELL_FOLDER_INVOKED,location);
        Ok(())
    }
    fn OnStateChange(&self,_:Ref<IShellView>,change:u32)->Result<()>{
        if [windows::Win32::UI::Shell::CDBOSC_SELCHANGE,
            windows::Win32::UI::Shell::CDBOSC_RENAME,
            windows::Win32::UI::Shell::CDBOSC_STATECHANGE].contains(&change) {
            self.selection_revision.set(self.selection_revision.get().wrapping_add(1));
        }
        // CDBOSC_SELCHANGE: coalesce bursts (select-all fires once per item).
        if change==windows::Win32::UI::Shell::CDBOSC_SELCHANGE && !self.selection_pending.replace(true) {
            let pending=self.selection_pending.clone();
            let _ = unsafe { PostMessageW(Some(HWND(self.hwnd as *mut _)), WM_SHELL_SELECTION_CHANGED, WPARAM(0), LPARAM(Rc::into_raw(pending) as isize)) };
        }
        Ok(())
    }
    fn IncludeObject(&self,_:Ref<IShellView>,_:*const windows::Win32::UI::Shell::Common::ITEMIDLIST)->Result<()>{Ok(())}
}
#[allow(non_snake_case)]
impl ICommDlgBrowser2_Impl for BrowserSite_Impl {
    fn Notify(&self,_:Ref<IShellView>,_:u32)->Result<()>{Ok(())}
    fn GetDefaultMenuText(&self,_:Ref<IShellView>,_:windows::core::PWSTR,_:i32)->Result<()>{Err(Error::from_hresult(S_FALSE))}
    fn GetViewFlags(&self)->Result<u32>{
        // Keep folder navigation interception without file-dialog "Select".
        Ok(windows::Win32::UI::Shell::CDB2GVF_NOINCLUDEITEM | windows::Win32::UI::Shell::CDB2GVF_NOSELECTVERB)
    }
}
#[implement(IExplorerBrowserEvents)]
struct BrowserEvents {
    hwnd: isize,
    browser: Weak<IExplorerBrowser>,
    modern: Rc<Cell<bool>>,
    selection_revision: Rc<Cell<u64>>,
    results_active: Rc<Cell<bool>>,
    results_location: Rc<RefCell<Option<ShellLocation>>>,
}

#[allow(non_snake_case)]
impl IExplorerBrowserEvents_Impl for BrowserEvents_Impl {
    fn OnNavigationPending(
        &self,
        _pidl_folder: *const windows::Win32::UI::Shell::Common::ITEMIDLIST,
    ) -> Result<()> {
        Ok(())
    }

    fn OnViewCreated(&self, shell_view: Ref<IShellView>) -> Result<()> {
        self.selection_revision.set(self.selection_revision.get().wrapping_add(1));
        if let Some(view) = shell_view.as_ref() {
            unsafe { super::menu::attach_view_menu(view, self.modern.clone()); }
            if self.results_active.get() { configure_results_columns(view); }
        }
        Ok(())
    }

    fn OnNavigationComplete(
        &self,
        pidl_folder: *const windows::Win32::UI::Shell::Common::ITEMIDLIST,
    ) -> Result<()> {
        self.selection_revision.set(self.selection_revision.get().wrapping_add(1));
        if self.results_active.get() {
            if let Some(browser) = self.browser.upgrade() {
                if let Ok(view) = unsafe { browser.GetCurrentView::<IShellView>() } { configure_results_columns(&view); }
            }
        }
        let logical = if self.results_active.get() { self.results_location.borrow().clone() } else { None };
        if let Some(location) = logical.or_else(|| unsafe { location_from_pidl(pidl_folder) }) {
            post_location(self.hwnd, WM_SHELL_NAVIGATED, location);
        }
        Ok(())
    }

    fn OnNavigationFailed(
        &self,
        _pidl_folder: *const windows::Win32::UI::Shell::Common::ITEMIDLIST,
    ) -> Result<()> {
        self.results_active.set(false);
        let _ = unsafe { PostMessageW(Some(HWND(self.hwnd as *mut _)), WM_SHELL_NAVIGATION_FAILED, WPARAM(0), LPARAM(0)) };
        Ok(())
    }
}

fn configure_results_columns(view: &IShellView) {
    // Presentation capability varies by Windows release; none of these
    // preferences may prevent the results folder from being usable.
    let Ok(folder) = view.cast::<IFolderView2>() else { return };
    if unsafe { folder.GetFolder::<IResultsFolder>() }.is_err() { return; }
    let _ = unsafe { folder.SetViewModeAndIconSize(FVM_DETAILS, 16) };
    let Ok(columns) = view.cast::<IColumnManager>() else { return };
    let mut keys = Vec::new();
    for name in ["System.ItemNameDisplay", "System.ItemFolderPathDisplay", "System.DateModified", "System.Size"] {
        let text: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        let mut key = PROPERTYKEY::default();
        if unsafe { windows::Win32::UI::Shell::PropertiesSystem::PSGetPropertyKeyFromName(windows::core::PCWSTR(text.as_ptr()), &mut key) }.is_err() { return; }
        keys.push(key);
    }
    let _ = unsafe { columns.SetColumns(&keys) };
}

fn post_location(hwnd: isize, message: u32, location: ShellLocation) {
    let pointer = Box::into_raw(Box::new(location));
    if unsafe { PostMessageW(Some(HWND(hwnd as *mut _)), message, WPARAM(0), LPARAM(pointer as isize)) }.is_err() {
        unsafe { drop(Box::from_raw(pointer)); }
    }
}
