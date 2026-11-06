//! Bounded per-tab snapshots; inactive tabs retain no Shell view or HWND.
use windows::Win32::{System::Com::CoTaskMemFree, UI::Shell::{
    Common::ITEMIDLIST, IFolderView2, ILGetSize, FOLDERVIEWMODE,
    SVSI_SELECT, SVSI_DESELECTOTHERS, SVSI_FOCUSED, SVSI_ENSUREVISIBLE,
}};
use windows::core::Interface;
use zsdesk_core::model::explorer::ShellViewState;
use super::ExplorerBrowserHost;

impl ExplorerBrowserHost {
    pub fn select_children(&self,names:&[std::ffi::OsString])->windows::core::Result<()> {
        use std::os::windows::ffi::OsStrExt;
        use windows::{core::PCWSTR,Win32::{Foundation::HWND,UI::Shell::IShellFolder}};
        let shell=unsafe{self.current_view()?};let view:IFolderView2=shell.cast()?;let folder:IShellFolder=unsafe{view.GetFolder()?};
        for(index,name)in names.iter().take(128).enumerate(){
            let name:Vec<u16>=name.encode_wide().chain(Some(0)).collect();let mut pidl=std::ptr::null_mut();
            unsafe{folder.ParseDisplayName(HWND::default(),None,PCWSTR(name.as_ptr()),None,&mut pidl,std::ptr::null_mut())?;}
            let flags=SVSI_SELECT.0|if index==0{SVSI_DESELECTOTHERS.0|SVSI_FOCUSED.0|SVSI_ENSUREVISIBLE.0}else{0};
            let result=unsafe{shell.SelectItem(pidl,flags as u32)};unsafe{CoTaskMemFree(Some(pidl.cast()))};result?;
        }
        Ok(())
    }
    pub fn select_child(&self, name:&std::ffi::OsStr) -> windows::core::Result<()> {
        use std::os::windows::ffi::OsStrExt;
        use windows::{core::PCWSTR,Win32::{Foundation::HWND,UI::Shell::IShellFolder}};
        let shell=unsafe {self.current_view()?};
        let view:IFolderView2=shell.cast()?;
        let folder:IShellFolder=unsafe {view.GetFolder()?};
        let name:Vec<u16>=name.encode_wide().chain(Some(0)).collect();
        let mut pidl=std::ptr::null_mut();
        unsafe {folder.ParseDisplayName(HWND::default(),None,PCWSTR(name.as_ptr()),None,&mut pidl,std::ptr::null_mut())?;}
        let result=unsafe {shell.SelectItem(pidl,(SVSI_SELECT.0|SVSI_DESELECTOTHERS.0|SVSI_FOCUSED.0|SVSI_ENSUREVISIBLE.0) as u32)};
        unsafe {CoTaskMemFree(Some(pidl.cast()))};
        result
    }
    pub fn view_mode(&self) -> Option<(i32,i32)> {
        let view: IFolderView2 = unsafe { self.current_view().ok()?.cast().ok()? };
        let mut mode=FOLDERVIEWMODE(0);let mut icon=0;
        unsafe { view.GetViewModeAndIconSize(&mut mode,&mut icon).ok()?; }
        Some((mode.0,icon))
    }
    pub fn set_view_mode(&self, mode:i32, icon:i32) -> windows::core::Result<()> {
        let view: IFolderView2 = unsafe { self.current_view()?.cast()? };
        unsafe { view.SetViewModeAndIconSize(FOLDERVIEWMODE(mode),icon) }
    }
    pub unsafe fn capture_view(&self) -> Option<ShellViewState> {
        let view: IFolderView2 = unsafe { self.current_view().ok()?.cast().ok()? };
        let mut mode=FOLDERVIEWMODE(0); let mut icon_size=0;
        unsafe { view.GetViewModeAndIconSize(&mut mode,&mut icon_size).ok()?; }
        let mut selected=Vec::new(); let mut last=-1; let mut budget=0;
        while selected.len()<256 {
            let Ok(index) = (unsafe { view.GetSelectedItem(last) }) else {break};
            if index <= last {break;} last=index;
            if let Some(bytes)=unsafe { item_bytes(&view,index) } {
                budget += bytes.len(); if budget > 128*1024 {break;}
                selected.push(bytes);
            }
        }
        let focused=unsafe { view.GetFocusedItem().ok().and_then(|i|item_bytes(&view,i)) };
        let first_visible=unsafe { view.GetVisibleItem(-1,false).ok().and_then(|i|item_bytes(&view,i)) };
        Some(ShellViewState {mode:mode.0,icon_size,selected,focused,first_visible})
    }
    pub unsafe fn restore_view(&self,state:&ShellViewState) {
        let Ok(shell_view)=(unsafe {self.current_view()}) else {return;};
        let Ok(view)=shell_view.cast::<IFolderView2>() else {return;};
        if (1..=8).contains(&state.mode) && (8..=512).contains(&state.icon_size) {
            let _=unsafe {view.SetViewModeAndIconSize(FOLDERVIEWMODE(state.mode),state.icon_size)};
        }
        // SelectItem without SVSI_SELECT can deselect the item. Scroll before
        // restoring the selection and preserve the focused item's own state.
        if let Some(pidl)=state.first_visible.as_ref().filter(|p|valid_pidl(p)) {
            let _=unsafe {shell_view.SelectItem(pidl.as_ptr().cast(),SVSI_ENSUREVISIBLE.0 as u32)};
        }
        let mut first=true;
        for pidl in state.selected.iter().take(256).filter(|p|valid_pidl(p)) {
            let flags=SVSI_SELECT.0 | if first {SVSI_DESELECTOTHERS.0} else {0};
            let _=unsafe {shell_view.SelectItem(pidl.as_ptr().cast(),flags as u32)}; first=false;
        }
        if let Some(pidl)=state.focused.as_ref().filter(|p|valid_pidl(p)) {
            let selection=if state.selected.contains(pidl){SVSI_SELECT.0}else{0};
            let _=unsafe {shell_view.SelectItem(pidl.as_ptr().cast(),(selection|SVSI_FOCUSED.0) as u32)};
        }
    }
}
unsafe fn item_bytes(view:&IFolderView2,index:i32)->Option<Vec<u8>> {
    let pointer: *mut ITEMIDLIST=unsafe {view.Item(index).ok()?};
    let len=unsafe {ILGetSize(Some(pointer))} as usize;
    let result=if (2..=8192).contains(&len) && !pointer.is_null() {
        Some(unsafe {std::slice::from_raw_parts(pointer.cast::<u8>(),len)}.to_vec())
    } else {None};
    unsafe {CoTaskMemFree(Some(pointer.cast()))}; result
}
fn valid_pidl(bytes:&[u8])->bool {
    if bytes.len()>8192 {return false;}
    let mut at=0;
    while at+2<=bytes.len() {
        let length=u16::from_le_bytes([bytes[at],bytes[at+1]]) as usize;
        if length==0 {return at+2==bytes.len();}
        if length<2 || at+length>bytes.len() {return false;}
        at+=length;
    }
    false
}
#[cfg(test)] mod tests {
    use super::*;
    #[test] fn rejects_truncated_or_unterminated_snapshot() {
        assert!(valid_pidl(&[4,0,1,2,0,0]));
        assert!(!valid_pidl(&[4,0,1,2])); assert!(!valid_pidl(&[1,0,0,0]));
        assert!(!valid_pidl(&[0,0,1])); assert!(!valid_pidl(&[]));
    }
}
