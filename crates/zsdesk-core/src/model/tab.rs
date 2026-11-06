//! 标签状态：单个标签维护独立的后退/前进历史。

use std::path::PathBuf;

/// 标签视图模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ViewMode {
    LargeIcons,
    MediumIcons,
    SmallIcons,
    List,
    Details,
}

/// 排序字段与方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SortField {
    Name,
    Kind,
    Size,
    Modified,
    Created,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SortDirection {
    Ascending,
    Descending,
}

/// 单个标签的完整状态。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TabState {
    pub tab_id: u64,
    pub location: PathBuf,
    pub back_stack: Vec<PathBuf>,
    pub forward_stack: Vec<PathBuf>,
    pub view_mode: ViewMode,
    pub sort_field: SortField,
    pub sort_direction: SortDirection,
    pub selection: Vec<u64>,
    pub scroll_anchor: Option<u64>,
    pub pinned: bool,
}

impl TabState {
    pub fn new(tab_id: u64, location: impl Into<PathBuf>) -> Self {
        Self {
            tab_id,
            location: location.into(),
            back_stack: Vec::new(),
            forward_stack: Vec::new(),
            view_mode: ViewMode::Details,
            sort_field: SortField::Name,
            sort_direction: SortDirection::Ascending,
            selection: Vec::new(),
            scroll_anchor: None,
            pinned: false,
        }
    }

    pub fn navigate(&mut self, next: PathBuf) {
        if next == self.location {
            return;
        }
        self.back_stack.push(self.location.clone());
        if self.back_stack.len() > 1024 {
            self.back_stack.remove(0);
        }
        self.forward_stack.clear();
        self.location = next;
        self.selection.clear();
        self.scroll_anchor = None;
    }

    pub fn can_go_back(&self) -> bool {
        !self.back_stack.is_empty()
    }

    pub fn can_go_forward(&self) -> bool {
        !self.forward_stack.is_empty()
    }

    pub fn back(&mut self) -> Option<PathBuf> {
        let prev = self.back_stack.pop()?;
        self.forward_stack.push(self.location.clone());
        self.location = prev;
        self.selection.clear();
        Some(self.location.clone())
    }

    pub fn forward(&mut self) -> Option<PathBuf> {
        let next = self.forward_stack.pop()?;
        self.back_stack.push(self.location.clone());
        self.location = next;
        self.selection.clear();
        Some(self.location.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn back_forward_round_trip() {
        let mut tab = TabState::new(1, "C:\\a");
        tab.navigate(PathBuf::from("C:\\a\\b"));
        tab.navigate(PathBuf::from("C:\\a\\b\\c"));
        assert!(tab.can_go_back());
        assert_eq!(tab.back(), Some(PathBuf::from("C:\\a\\b")));
        assert!(tab.can_go_forward());
        assert_eq!(tab.forward(), Some(PathBuf::from("C:\\a\\b\\c")));
    }

    #[test]
    fn navigate_clears_forward_stack() {
        let mut tab = TabState::new(1, "C:\\a");
        tab.navigate(PathBuf::from("C:\\a\\b"));
        tab.back();
        tab.navigate(PathBuf::from("C:\\d"));
        assert!(!tab.can_go_forward());
    }

    #[test]
    fn navigating_to_same_location_is_noop() {
        let mut tab = TabState::new(1, "C:\\a");
        tab.navigate(PathBuf::from("C:\\a"));
        assert!(!tab.can_go_back());
    }
}
