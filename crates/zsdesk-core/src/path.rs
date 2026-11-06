//! 路径抽象：Windows 文件不能只用路径识别。
//!
//! 可用时同时保存卷标识与文件 ID；路径用于显示和无法取得稳定 ID 时的回退。

use std::path::PathBuf;

/// 文件身份：稳定 ID + 路径回退。
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct FileIdentity {
    /// 卷序列号（Windows VolumeSerialNumber）。
    pub volume_id: Option<u64>,
    /// 文件 ID（NTFS FRN / 稳定标识）。
    pub file_id: Option<u64>,
    /// 路径回退（显示与不稳定介质）。
    pub path: PathBuf,
}

impl FileIdentity {
    pub fn from_path(path: impl Into<PathBuf>) -> Self {
        Self {
            volume_id: None,
            file_id: None,
            path: path.into(),
        }
    }

    pub fn with_ids(mut self, volume_id: u64, file_id: u64) -> Self {
        self.volume_id = Some(volume_id);
        self.file_id = Some(file_id);
        self
    }

    /// 仅凭路径是否可用作稳定身份。
    pub fn is_path_only(&self) -> bool {
        self.file_id.is_none()
    }
}

/// 规范化相对路径片段（用于显示与排序）。
pub fn normalize_display_name(name: &str) -> String {
    name.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_falls_back_to_path() {
        let id = FileIdentity::from_path("C:\\Users\\x\\a.txt");
        assert!(id.is_path_only());
        assert_eq!(id.path.to_string_lossy(), "C:\\Users\\x\\a.txt");
    }

    #[test]
    fn identity_with_ids_is_stable() {
        let id = FileIdentity::from_path("C:\\a.txt").with_ids(0x1234, 0xABCD);
        assert!(!id.is_path_only());
    }
}
