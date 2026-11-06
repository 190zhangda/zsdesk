//! Bounded per-tab snapshots; inactive tabs retain no Shell view or HWND.
use windows::Win32::{System::Com::CoTaskMemFree, UI::Shell::{
    Common::ITEMIDLIST, IEnumIDList, IFolderView2, ILGetSize, FOLDERVIEWMODE, SVGIO_SELECTION,
    SVSI_SELECT, SVSI_DESELECTOTHERS, SVSI_FOCUSED, SVSI_ENSUREVISIBLE,
}};
use windows::core::{HRESULT, Interface};
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
        let mut selected=Vec::new(); let mut budget=0;
        // Some native DefView builds return the first selected index regardless
        // of GetSelectedItem's start argument. Enumerate the selected child PIDLs
        // directly, preserving their identity within virtual folders as well.
        if let Ok(items)=unsafe {view.Items::<IEnumIDList>(SVGIO_SELECTION)} {
            for _ in 0..256 {
                let mut pointers=[std::ptr::null_mut()];let mut fetched=0;
                let status=unsafe {items.Next(&mut pointers,Some(&mut fetched))};
                // Failed enumerations do not transfer valid output entries.
                if status.is_err() || fetched!=1 {break;}
                if let Some(bytes)=unsafe {take_item_bytes(pointers[0])} {
                    budget += bytes.len(); if budget > 128*1024 {break;}
                    selected.push(bytes);
                }
                if status!=HRESULT(0) {break;}
            }
        }
        let mut focused_index=-1;
        let focused_status=unsafe {(view.vtable().base__.GetFocusedItem)(view.as_raw(),&mut focused_index)};
        let focused=if focused_status==HRESULT(0) && focused_index>=0 {unsafe {item_bytes(&view,focused_index)}}else{None};
        let mut visible_index=-1;
        let visible_status=unsafe {(view.vtable().GetVisibleItem)(view.as_raw(),-1,false.into(),&mut visible_index)};
        let first_visible=if visible_status==HRESULT(0) && visible_index>=0 {unsafe {item_bytes(&view,visible_index)}}else{None};
        Some(ShellViewState {mode:mode.0,icon_size,selected,focused,first_visible})
    }
    pub unsafe fn restore_view(&self,state:&ShellViewState) {
        let Ok(shell_view)=(unsafe {self.current_view()}) else {return;};
        let Ok(view)=shell_view.cast::<IFolderView2>() else {return;};
        if (1..=8).contains(&state.mode) && (8..=512).contains(&state.icon_size) {
            let mut mode=FOLDERVIEWMODE(0);let mut icon=0;
            let unchanged=unsafe {view.GetViewModeAndIconSize(&mut mode,&mut icon)}.is_ok()
                && (mode.0,icon)==(state.mode,state.icon_size);
            if !unchanged {let _=unsafe {view.SetViewModeAndIconSize(FOLDERVIEWMODE(state.mode),state.icon_size)};}
        }
        // Clear once. Passing DESELECTOTHERS to SelectAndPositionItems applies
        // it to every item and leaves only the last item selected on DefView.
        let _=unsafe {shell_view.SelectItem(std::ptr::null(),SVSI_DESELECTOTHERS.0 as u32)};
        // The SDK's SVSI_NOSTATECHANGE macro is outside the _SVSIF enum.
        const NO_STATE_CHANGE:u32=0x8000_0000;
        if let Some(pidl)=state.first_visible.as_ref().filter(|p|valid_pidl(p)) {
            let _=unsafe {shell_view.SelectItem(pidl.as_ptr().cast(),SVSI_ENSUREVISIBLE.0 as u32|NO_STATE_CHANGE)};
        }
        let mut selected:Vec<_>=state.selected.iter().take(256).filter(|p|valid_pidl(p)).collect();
        let mut flags=SVSI_SELECT.0 as u32;
        if let Some(pidl)=state.focused.as_ref().filter(|p|valid_pidl(p)) {
            if let Some(index)=selected.iter().position(|selected|*selected==pidl) {
                // DefView focuses the final item in a batch. Keep this in the
                // same submission so a later single-item request cannot replace
                // the pending selection while a newly created view enumerates.
                let focused=selected.remove(index);selected.push(focused);
                flags|=SVSI_FOCUSED.0 as u32;
            } else {
                let _=unsafe {shell_view.SelectItem(pidl.as_ptr().cast(),SVSI_FOCUSED.0 as u32)};
            }
        }
        if !selected.is_empty() {
            let pidls:Vec<*const ITEMIDLIST>=selected.iter().map(|p|p.as_ptr().cast()).collect();
            let _=unsafe {view.SelectAndPositionItems(pidls.len() as u32,pidls.as_ptr(),None,flags)};
        }
    }
}
unsafe fn item_bytes(view:&IFolderView2,index:i32)->Option<Vec<u8>> {
    let pointer: *mut ITEMIDLIST=unsafe {view.Item(index).ok()?};
    unsafe {take_item_bytes(pointer)}
}
unsafe fn take_item_bytes(pointer:*mut ITEMIDLIST)->Option<Vec<u8>> {
    if pointer.is_null() {return None;}
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
