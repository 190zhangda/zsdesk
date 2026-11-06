//! File shortcuts are separate from menu visibility and native text editing.

use windows::{core::{Interface, Result}, Win32::{Foundation::HWND,
    UI::{Input::KeyboardAndMouse::*, Shell::{IFolderView2, IShellView, SVGIO_ALLVIEW, SVGIO_SELECTION, SVSI_SELECT}}}};
use zsdesk_windows::shell::menu::ShellMenu;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Modifiers { pub control: bool, pub shift: bool, pub alt: bool }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FileShortcut { Copy, Cut, Paste, SelectAll, Rename, Delete { permanent: bool }, Properties, Undo, Redo }

pub(super) fn resolve(key: u16, modifiers: Modifiers, file_context: bool, text_editing: bool) -> Option<FileShortcut> {
    if !file_context || text_editing { return None; }
    let Modifiers { control, shift, alt } = modifiers;
    if control && !shift && !alt {
        return match key {
            key if key == VK_C.0 || key == VK_INSERT.0 => Some(FileShortcut::Copy),
            key if key == VK_X.0 => Some(FileShortcut::Cut),
            key if key == VK_V.0 => Some(FileShortcut::Paste),
            key if key == VK_A.0 => Some(FileShortcut::SelectAll),
            key if key == VK_Z.0 => Some(FileShortcut::Undo),
            key if key == VK_Y.0 => Some(FileShortcut::Redo),
            _ => None,
        };
    }
    if shift && !control && !alt && key == VK_INSERT.0 { return Some(FileShortcut::Paste); }
    if !control && !alt && key == VK_DELETE.0 { return Some(FileShortcut::Delete { permanent: shift }); }
    if !control && !shift && !alt && key == VK_F2.0 { return Some(FileShortcut::Rename); }
    if alt && !control && !shift && key == VK_RETURN.0 { return Some(FileShortcut::Properties); }
    None
}

/// Must run on the view's STA without holding the application state RefCell.
pub(super) fn execute(view: &IShellView, owner: HWND, action: FileShortcut) -> Result<()> {
    if matches!(action, FileShortcut::Copy | FileShortcut::Cut | FileShortcut::Rename | FileShortcut::Delete { .. } | FileShortcut::Properties) {
        if let Ok(folder) = view.cast::<IFolderView2>() {
            if unsafe { folder.ItemCount(SVGIO_SELECTION) }.is_ok_and(|count| count == 0) { return Ok(()); }
        }
    }
    if action == FileShortcut::Rename { return unsafe { view.cast::<IFolderView2>()?.DoRename() }; }
    if action == FileShortcut::SelectAll {
        let folder: IFolderView2 = view.cast()?;
        let count = unsafe { folder.ItemCount(SVGIO_ALLVIEW)? }.max(0);
        for index in 0..count { unsafe { folder.SelectItem(index, SVSI_SELECT.0 as u32)?; } }
        return Ok(());
    }
    let (selection, verb, shift) = match action {
        FileShortcut::Copy => (true, "copy", false), FileShortcut::Cut => (true, "cut", false),
        FileShortcut::Paste => (false, "paste", false), FileShortcut::Delete { permanent } => (true, "delete", permanent),
        FileShortcut::Properties => (true, "properties", false), FileShortcut::Undo => (false, "undo", false),
        FileShortcut::Redo => (false, "redo", false), _ => unreachable!(),
    };
    let menu = ShellMenu::for_view(view, selection)?;
    menu.populate_for_invoke(shift)?;
    menu.invoke_verb(owner, verb, shift)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn common_file_keys_do_not_depend_on_a_menu_or_native_accelerator() {
        let ctrl = Modifiers { control: true, ..Default::default() };
        for (key, action) in [(VK_C, FileShortcut::Copy), (VK_X, FileShortcut::Cut), (VK_V, FileShortcut::Paste),
            (VK_A, FileShortcut::SelectAll), (VK_Z, FileShortcut::Undo), (VK_Y, FileShortcut::Redo)] {
            assert_eq!(resolve(key.0, ctrl, true, false), Some(action));
        }
        assert_eq!(resolve(VK_F2.0, Modifiers::default(), true, false), Some(FileShortcut::Rename));
        assert_eq!(resolve(VK_DELETE.0, Modifiers::default(), true, false), Some(FileShortcut::Delete { permanent: false }));
        assert_eq!(resolve(VK_DELETE.0, Modifiers { shift: true, ..Default::default() }, true, false), Some(FileShortcut::Delete { permanent: true }));
    }
    #[test]
    fn editors_and_other_panes_keep_their_native_shortcuts() {
        for key in [VK_C, VK_X, VK_V, VK_A, VK_Z, VK_Y, VK_DELETE, VK_F2] {
            for control in [false, true] {
                let keys = Modifiers { control, ..Default::default() };
                assert_eq!(resolve(key.0, keys, true, true), None);
                assert_eq!(resolve(key.0, keys, false, false), None);
            }
        }
        assert_eq!(resolve(VK_C.0, Modifiers { control: true, shift: true, alt: false }, true, false), None);
    }
}
