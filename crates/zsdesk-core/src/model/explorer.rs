//! Explorer-compatible locations, tabs, bookmarks, and session state.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ShellLocation {
    ThisPc,
    Path(PathBuf),
    Shell { parsing_name: String, title: String },
    /// Stable query parameters; transient Shell search addresses are not persisted.
    Search { query: String, scope: Option<PathBuf> },
}

impl ShellLocation {
    pub fn title(&self) -> String {
        match self {
            Self::ThisPc => "此电脑".to_string(),
            Self::Path(path) => path_title(path),
            Self::Shell { parsing_name, title } => shell_display_title(parsing_name, title),
            Self::Search { query, .. } => format!("搜索：{query}"),
        }
    }

    pub fn address(&self) -> String {
        match self {
            Self::ThisPc => "此电脑".to_string(),
            Self::Path(path) => path.to_string_lossy().into_owned(),
            Self::Shell { parsing_name, .. } => parsing_name.clone(),
            Self::Search { query, scope } => format!("搜索 {}：{}", scope.as_ref().map(|p|p.display().to_string()).unwrap_or_else(||"全部本地磁盘".into()), query),
        }
    }

    pub fn as_path(&self) -> Option<&Path> {
        match self {
            Self::Path(path) => Some(path),
            _ => None,
        }
    }
}

fn shell_display_title(parsing_name: &str, title: &str) -> String {
    let title = title.trim();
    if !title.is_empty() && !title.eq_ignore_ascii_case("shell") && !title.starts_with("::{")
        && !title.to_ascii_lowercase().starts_with("shell:") && !title.eq_ignore_ascii_case(parsing_name) {
        return title.to_string();
    }
    let name = parsing_name.get(..6).filter(|prefix| prefix.eq_ignore_ascii_case("shell:"))
        .map_or(parsing_name, |_| &parsing_name[6..]);
    let known = [
        ("::{645FF040-5081-101B-9F08-00AA002F954E}", "回收站"), ("RecycleBinFolder", "回收站"),
        ("::{20D04FE0-3AEA-1069-A2D8-08002B30309D}", "此电脑"), ("MyComputerFolder", "此电脑"),
        ("::{F02C1A0D-BE21-4350-88B0-7367FC96EF3C}", "网络"), ("NetworkPlacesFolder", "网络"),
        ("::{679F85CB-0220-4080-B29B-5540CC05AAB6}", "主文件夹"),
        ("::{F874310E-B6B7-47DC-BC84-B9E6B38F5903}", "主文件夹"),
        ("::{031E4825-7B94-4DC3-B131-E946B44C8DD5}", "库"), ("Libraries", "库"),
        ("::{26EE0668-A00A-44D7-9371-BEB064C98683}", "控制面板"), ("ControlPanelFolder", "控制面板"),
        ("Desktop", "桌面"), ("Downloads", "下载"),
    ];
    known.into_iter().find(|(candidate, _)| name.eq_ignore_ascii_case(candidate))
        .map_or("系统位置", |(_, label)| label).to_string()
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExplorerTab {
    pub id: u64,
    history: Vec<ShellLocation>,
    history_index: usize,
    #[serde(default)]
    pub view: Option<ShellViewState>,
}

impl ExplorerTab {
    pub fn new(id: u64, location: ShellLocation) -> Self {
        Self {
            id,
            history: vec![location],
            history_index: 0,
            view: None,
        }
    }

    pub fn current(&self) -> &ShellLocation {
        self.history
            .get(self.history_index)
            .unwrap_or(&self.history[0])
    }

    pub fn navigate(&mut self, location: ShellLocation) -> bool {
        if self.current() == &location {
            return false;
        }
        self.history.truncate(self.history_index + 1);
        self.history.push(location);
        if self.history.len() > 100 { self.history.remove(0); }
        self.history_index = self.history.len() - 1;
        self.view = None;
        true
    }

    pub fn replace_current(&mut self, location: ShellLocation) {
        self.normalize();
        self.history[self.history_index] = location;
    }

    pub fn can_go_back(&self) -> bool {
        self.history_index > 0
    }

    pub fn can_go_forward(&self) -> bool {
        self.history_index + 1 < self.history.len()
    }

    pub fn go_back(&mut self) -> Option<&ShellLocation> {
        if !self.can_go_back() {
            return None;
        }
        self.history_index -= 1;
        Some(self.current())
    }

    pub fn go_forward(&mut self) -> Option<&ShellLocation> {
        if !self.can_go_forward() {
            return None;
        }
        self.history_index += 1;
        Some(self.current())
    }

    pub fn normalize(&mut self) {
        if self.history.is_empty() {
            self.history.push(ShellLocation::ThisPc);
        }
        self.history_index = self.history_index.min(self.history.len() - 1);
    }
}

/// Per-tab view settings and relative Shell identities. No live COM objects are retained.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShellViewState {
    pub mode: i32,
    pub icon_size: i32,
    pub selected: Vec<Vec<u8>>,
    pub focused: Option<Vec<u8>>,
    pub first_visible: Option<Vec<u8>>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Bookmark {
    pub id: u64,
    pub display_name: String,
    pub target: ShellLocation,
}

/// Restored window rectangle in screen pixels.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct WindowPlacement {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub maximized: bool,
}

/// Individually configurable actions in the Explorer command bar.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum CommandBarItem { New, Cut, Copy, Paste, Rename, Share, Delete, Sort, View, More, PreviewPane, DetailsPane }

impl CommandBarItem {
    pub const ALL: [Self; 11] = [Self::New, Self::Cut, Self::Copy, Self::Paste, Self::Rename, Self::Share,
        Self::Delete, Self::Sort, Self::View, Self::More, Self::PreviewPane];

    pub const fn key(self) -> &'static str {
        match self {
            Self::New => "new", Self::Cut => "cut", Self::Copy => "copy", Self::Paste => "paste",
            Self::Rename => "rename", Self::Share => "share", Self::Delete => "delete", Self::Sort => "sort",
            Self::View => "view", Self::More => "more", Self::PreviewPane => "preview-pane", Self::DetailsPane => "details-pane",
        }
    }

    pub fn from_key(value: &str) -> Option<Self> { Self::ALL.into_iter().find(|item| item.key() == value) }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct CommandBarPreferences {
    pub visible: bool,
    pub hidden_items: Vec<CommandBarItem>,
}

impl Default for CommandBarPreferences {
    fn default() -> Self { Self { visible: true, hidden_items: Vec::new() } }
}

impl CommandBarPreferences {
    pub fn item_visible(&self, item: CommandBarItem) -> bool { item != CommandBarItem::DetailsPane && !self.hidden_items.contains(&item) }
    pub fn set_item_visible(&mut self, item: CommandBarItem, visible: bool) {
        self.hidden_items.retain(|hidden| *hidden != item);
        if !visible { self.hidden_items.push(item); }
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum ContextMenuMode {
    #[serde(rename = "original")]
    Original,
    #[default]
    #[serde(rename = "windows11")]
    Windows11,
}

/// Menu visibility does not disable commands or keyboard shortcuts.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ContextMenuPreferences {
    pub mode: ContextMenuMode,
    pub show_cut_copy_paste: bool,
    pub show_copy_path: bool,
    pub show_rename: bool,
    pub show_delete: bool,
    pub show_properties: bool,
    pub show_share: bool,
    pub show_new: bool,
    pub show_seven_zip: bool,
    pub show_wps: bool,
}

impl Default for ContextMenuPreferences {
    fn default() -> Self {
        Self { mode: ContextMenuMode::Windows11, show_cut_copy_paste: true, show_copy_path: true,
            show_rename: true, show_delete: true, show_properties: true, show_share: true,
            show_new: true, show_seven_zip: true, show_wps: false }
    }
}

/// Chrome preferences shared by every tab.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ExplorerUi {
    pub show_tree: bool,
    /// Navigation tree width in DIP.
    pub tree_width: i32,
    pub show_bookmarks: bool,
    pub show_preview: bool,
    pub show_details: bool,
    pub command_bar: CommandBarPreferences,
    pub context_menu: ContextMenuPreferences,
    pub search_all_files: bool,
    pub space_preview: bool,
    pub window: Option<WindowPlacement>,
}

impl Default for ExplorerUi {
    fn default() -> Self {
        Self { show_tree: true, tree_width: 220, show_bookmarks: true, show_preview: false, show_details: false,
            command_bar: CommandBarPreferences::default(), context_menu: ContextMenuPreferences::default(),
            search_all_files: true, space_preview: true, window: None }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExplorerSession {
    pub bookmarks: Vec<Bookmark>,
    pub tabs: Vec<ExplorerTab>,
    pub active_tab: usize,
    pub next_id: u64,
    #[serde(default)]
    pub ui: ExplorerUi,
}

impl Default for ExplorerSession {
    fn default() -> Self {
        Self {
            bookmarks: Vec::new(),
            tabs: vec![ExplorerTab::new(1, ShellLocation::ThisPc)],
            active_tab: 0,
            next_id: 2,
            ui: ExplorerUi::default(),
        }
    }
}

impl ExplorerSession {
    pub fn normalize(&mut self) {
        if self.tabs.is_empty() {
            let id = self.take_id();
            self.tabs.push(ExplorerTab::new(id, ShellLocation::ThisPc));
        }
        for tab in &mut self.tabs {
            tab.normalize();
        }
        self.active_tab = self.active_tab.min(self.tabs.len() - 1);
        self.ui.tree_width = self.ui.tree_width.clamp(120, 800);
        let largest = self.tabs.iter().map(|t|t.id)
            .chain(self.bookmarks.iter().map(|b|b.id)).max().unwrap_or(0);
        self.next_id = self.next_id.max(largest.saturating_add(1));
    }

    pub fn active_tab(&self) -> &ExplorerTab {
        &self.tabs[self.active_tab]
    }

    pub fn active_tab_mut(&mut self) -> &mut ExplorerTab {
        &mut self.tabs[self.active_tab]
    }

    pub fn current_location(&self) -> &ShellLocation {
        self.active_tab().current()
    }

    pub fn take_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        id
    }
}

pub fn path_title(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_namespace_titles_are_readable_without_rewriting_addresses() {
        let address = "::{645FF040-5081-101B-9F08-00AA002F954E}";
        let location = ShellLocation::Shell { parsing_name: address.into(), title: address.into() };
        assert_eq!(location.title(), "回收站");
        assert_eq!(location.address(), address);
        let restored: ShellLocation = serde_json::from_str(&serde_json::to_string(&location).unwrap()).unwrap();
        assert_eq!(restored.title(), "回收站");
        assert_eq!(ShellLocation::Shell { parsing_name: "shell:RecycleBinFolder".into(), title: "Shell".into() }.title(), "回收站");
        assert_eq!(ShellLocation::Shell { parsing_name: address.into(), title: "已删除的文件".into() }.title(), "已删除的文件");
        assert_eq!(ShellLocation::Shell { parsing_name: "::{11111111-1111-1111-1111-111111111111}".into(), title: String::new() }.title(), "系统位置");
    }

    #[test]
    fn shell_locations_participate_in_tab_history() {
        let mut tab = ExplorerTab::new(1, ShellLocation::ThisPc);
        let drive = ShellLocation::Path(PathBuf::from(r"D:\"));
        assert!(tab.navigate(drive.clone()));
        assert_eq!(tab.go_back(), Some(&ShellLocation::ThisPc));
        assert_eq!(tab.go_forward(), Some(&drive));
    }

    #[test]
    fn navigating_to_current_location_does_not_duplicate_history() {
        let mut tab = ExplorerTab::new(1, ShellLocation::ThisPc);
        assert!(!tab.navigate(ShellLocation::ThisPc));
        assert!(!tab.can_go_back());
    }

    #[test]
    fn sessions_without_ui_section_still_load() {
        let text = r#"{"bookmarks":[],"tabs":[{"id":1,"history":["ThisPc"],"history_index":0}],"active_tab":0,"next_id":2}"#;
        let session: ExplorerSession = serde_json::from_str(text).unwrap();
        assert!(session.ui.show_tree && session.ui.show_bookmarks);
        assert_eq!(session.ui.tree_width, 220);
    }

    #[test]
    fn older_ui_preferences_keep_new_defaults_without_losing_existing_choices() {
        let old = r#"{"show_tree":false,"tree_width":268,"show_bookmarks":false,"show_preview":true,"show_details":false,"window":null}"#;
        let ui: ExplorerUi = serde_json::from_str(old).unwrap();
        assert!(!ui.show_tree && !ui.show_bookmarks && ui.show_preview);
        assert_eq!(ui.tree_width, 268);
        assert_eq!(ui.command_bar, CommandBarPreferences::default());
        assert_eq!(ui.context_menu, ContextMenuPreferences::default());
        assert!(ui.search_all_files && ui.space_preview);
    }

    #[test]
    fn customized_ui_preferences_survive_serialization() {
        let mut ui = ExplorerUi::default();
        ui.command_bar.visible = false;
        ui.command_bar.set_item_visible(CommandBarItem::Share, false);
        ui.command_bar.set_item_visible(CommandBarItem::PreviewPane, false);
        ui.context_menu.mode = ContextMenuMode::Original;
        ui.context_menu.show_seven_zip = false;
        ui.search_all_files = false;
        ui.space_preview = false;
        let restored: ExplorerUi = serde_json::from_str(&serde_json::to_string(&ui).unwrap()).unwrap();
        assert_eq!(restored, ui);
        assert!(restored.command_bar.item_visible(CommandBarItem::New));
        assert!(!restored.command_bar.item_visible(CommandBarItem::PreviewPane));
    }

    #[test]
    fn session_normalization_recovers_empty_tabs() {
        let mut session = ExplorerSession {
            tabs: Vec::new(),
            ..ExplorerSession::default()
        };
        session.normalize();
        assert_eq!(session.tabs.len(), 1);
        assert_eq!(session.current_location(), &ShellLocation::ThisPc);
    }
}
