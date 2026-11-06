//! Owned PIDL bytes. ZSDesk keeps Shell identities as plain byte vectors so no
//! COM allocation outlives the call that produced it.

use windows::{
    core::Result,
    Win32::{
        System::Com::CoTaskMemFree,
        UI::Shell::{Common::ITEMIDLIST, ILGetSize, IShellItem, SHCreateItemFromIDList, SHGetIDListFromObject},
    },
};
use zsdesk_core::model::explorer::ShellLocation;

/// Copies an ITEMIDLIST into owned bytes (including the terminator).
pub fn pidl_bytes(pidl: *const ITEMIDLIST) -> Vec<u8> {
    if pidl.is_null() { return vec![0, 0]; }
    let len = unsafe { ILGetSize(Some(pidl)) } as usize;
    if len < 2 { return vec![0, 0]; }
    unsafe { std::slice::from_raw_parts(pidl.cast::<u8>(), len) }.to_vec()
}

pub fn item_pidl(item: &IShellItem) -> Option<Vec<u8>> {
    let pidl = unsafe { SHGetIDListFromObject(item).ok()? };
    let bytes = pidl_bytes(pidl);
    unsafe { CoTaskMemFree(Some(pidl.cast())) };
    Some(bytes)
}

pub fn item_from_pidl(pidl: &[u8]) -> Result<IShellItem> {
    unsafe { SHCreateItemFromIDList(pidl.as_ptr().cast()) }
}

pub fn location_pidl(location: &ShellLocation) -> Option<Vec<u8>> {
    let item = unsafe { super::shell_item(location).ok()? };
    item_pidl(&item)
}

