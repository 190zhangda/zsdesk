use super::*;
use zsdesk_core::model::explorer::{Bookmark, path_title};
use zsdesk_ui::menus::{entry, menu, MenuItemSpec};
use zsdesk_windows::shell::open_path;
use super::navigation::window_text;

impl WindowState {
    pub(super) fn add_current_bookmark(&mut self) {
        let target = self.current_location().clone();
        self.add_bookmark(target.title(), target);
    }
    pub(super) fn add_bookmark_path(&mut self, path: PathBuf) { self.add_bookmark(path_title(&path), ShellLocation::Path(path)); }
    pub(super) fn add_bookmark(&mut self, name: String, target: ShellLocation) {
        if self.session.bookmarks.iter().any(|b| b.target == target) {
            self.show_message(format!("“{name}”已在收藏栏中"));
            return;
        }
        let id = self.session.take_id();
        self.session.bookmarks.push(Bookmark { id, display_name: name, target });
        self.session.ui.show_bookmarks = true;
        self.save();
        unsafe { self.rebuild_bookmarks(); }
    }
    pub(super) unsafe fn rebuild_bookmarks(&mut self) {
        self.measure_bookmarks();
        unsafe { self.resize(); }
    }
    pub(super) unsafe fn open_bookmark(&mut self, index: usize, new_tab: bool) {
        let Some(bookmark) = self.session.bookmarks.get(index).cloned() else { return };
        if let ShellLocation::Path(path) = &bookmark.target {
            if path.is_file() { unsafe { open_path(self.hwnd, path); } return; }
        }
        if new_tab { unsafe { self.new_tab(bookmark.target); } } else { unsafe { self.navigate_to(bookmark.target); } }
    }
    pub(super) unsafe fn bookmark_menu(&mut self, index: usize) {
        if index >= self.session.bookmarks.len() { return; }
        let last = self.session.bookmarks.len() - 1;
        let spec = menu(vec![
            entry("打开", "open", true, false),
            entry("在新标签页中打开", "open-tab", true, false),
            MenuItemSpec::Separator,
            entry("重命名", "rename", true, false),
            entry("向左移动", "left", index > 0, false),
            entry("向右移动", "right", index < last, false),
            MenuItemSpec::Separator,
            entry("从收藏栏移除", "remove", true, false),
        ]);
        match input::popup(self.hwnd, &spec, None).as_deref() {
            Some("open") => unsafe { self.open_bookmark(index, false) },
            Some("open-tab") => unsafe { self.open_bookmark(index, true) },
            Some("rename") => unsafe { self.start_bookmark_rename(index) },
            Some("left") => self.move_bookmark(index, index.saturating_sub(1)),
            Some("right") => self.move_bookmark(index, (index + 1).min(last)),
            Some("remove") => { self.session.bookmarks.remove(index); self.save(); unsafe { self.rebuild_bookmarks(); } }
            _ => {}
        }
    }
    pub(super) unsafe fn bookmarks_overflow(&mut self) {
        let hidden = self.layout.bookmarks.len();
        let mut items: Vec<MenuItemSpec> = self.session.bookmarks.iter().enumerate().skip(hidden)
            .map(|(i, b)| entry(&b.display_name.replace('&', "&&"), &format!("bm-{i}"), true, false)).collect();
        if items.is_empty() { items.push(entry("收藏当前文件夹\tCtrl+D", "add", true, false)); }
        match input::popup(self.hwnd, &menu(items), Some(self.layout.bookmark_overflow)).as_deref() {
            Some("add") => self.add_current_bookmark(),
            Some(id) => if let Some(i) = id.strip_prefix("bm-").and_then(|n| n.parse().ok()) { unsafe { self.open_bookmark(i, false); } },
            None => {}
        }
    }
    pub(super) unsafe fn bookmarks_bar_menu(&mut self) {
        let spec = menu(vec![
            entry("收藏当前文件夹\tCtrl+D", "add", true, false),
            MenuItemSpec::Separator,
            entry("显示收藏栏", "toggle", true, self.session.ui.show_bookmarks),
        ]);
        match input::popup(self.hwnd, &spec, None).as_deref() {
            Some("add") => self.add_current_bookmark(),
            Some("toggle") => { self.session.ui.show_bookmarks = !self.session.ui.show_bookmarks; self.save(); unsafe { self.resize(); } }
            _ => {}
        }
    }
    unsafe fn start_bookmark_rename(&mut self, index: usize) {
        unsafe { self.commit_bookmark_rename(); }
        let Some(bookmark) = self.session.bookmarks.get(index) else { return };
        let Some(rect) = self.layout.bookmarks.get(index).copied() else { return };
        self.rename_hwnd = unsafe { child(self.hwnd, "EDIT", &bookmark.display_name, ID_BOOKMARK_RENAME, ES_AUTOHSCROLL as u32, true) }.unwrap_or_default();
        self.rename_index = Some(index);
        // Borderless EDIT inside the rounded text box painted by the chrome.
        let line = self.edit_line_height();
        let pad = self.s(8);
        let r = LayoutRect::new(rect.x + pad, rect.y + (rect.height - line) / 2, rect.width.max(self.s(140)) - pad * 2, line);
        unsafe {
            move_control(self.rename_hwnd, r);
            SendMessageW(self.rename_hwnd, WM_SETFONT, Some(WPARAM(self.font.0 as usize)), Some(LPARAM(1)));
            if !SetWindowSubclass(self.rename_hwnd, Some(edit_proc), ID_BOOKMARK_RENAME, self.hwnd.0 as usize).as_bool() { self.cancel_rename(); return; }
            SendMessageW(self.rename_hwnd, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1)));
            let _ = SetFocus(Some(self.rename_hwnd));
        }
        self.invalidate_rect(self.layout.bookmarks_row);
    }
    pub(super) unsafe fn commit_bookmark_rename(&mut self) {
        let Some(index) = self.rename_index.take() else { return };
        let text = window_text(self.rename_hwnd);
        let _ = unsafe { DestroyWindow(self.rename_hwnd) };
        self.rename_hwnd = HWND::default();
        if !text.trim().is_empty() {
            if let Some(b) = self.session.bookmarks.get_mut(index) { b.display_name = text.trim().into(); self.save(); }
        }
        unsafe { self.rebuild_bookmarks(); }
    }
    pub(super) unsafe fn cancel_rename(&mut self) {
        self.rename_index = None;
        if !self.rename_hwnd.0.is_null() { let _ = unsafe { DestroyWindow(self.rename_hwnd) }; self.rename_hwnd = HWND::default(); }
        self.invalidate_rect(self.layout.bookmarks_row);
    }
    pub(super) fn move_bookmark(&mut self, from: usize, to: usize) {
        if from == to || from >= self.session.bookmarks.len() || to >= self.session.bookmarks.len() { return; }
        let b = self.session.bookmarks.remove(from);
        self.session.bookmarks.insert(to, b);
        self.save();
        unsafe { self.rebuild_bookmarks(); }
    }
}
