//! Shell 与文件系统抽象：Windows 实现的契约。
//!
//! 设计约束（规划 §8.1）：
//! - 领域层不依赖 HWND、COM 指针；
//! - 耗时操作带取消令牌和请求代号；
//! - 文件操作与搜索索引解耦。

use std::path::{Path, PathBuf};
use zsdesk_core::error::Result;
use zsdesk_core::model::view::{FileEntry, RequestId};

/// 取消令牌：操作可被取消。
#[derive(Debug, Clone, Default)]
pub struct CancellationToken {
    cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// 目录枚举结果（批次）。
#[derive(Debug, Clone)]
pub struct ListingBatch {
    pub request_id: RequestId,
    pub entries: Vec<FileEntry>,
    pub total: usize,
    pub has_more: bool,
}

/// 目录枚举器：平台实现。
pub trait DirectoryLister: Send {
    /// 枚举目录；`limit_offset/limit_length` 为可见区提示。
    fn list(
        &self,
        path: &Path,
        request_id: RequestId,
        offset: usize,
        length: usize,
        cancel: &CancellationToken,
    ) -> Result<ListingBatch>;
}

/// 系统图标与缩略图获取：按需 + LRU 缓存。
pub trait IconProvider: Send {
    /// 取文件类型图标（RGBA 像素）。
    fn file_icon(&self, path: &Path, size: u32) -> Option<Vec<u8>>;
}

/// 文件操作执行器：使用 IFileOperation 保留系统语义。
pub trait FileOperator: Send {
    /// 移动/复制/删除，返回是否成功。
    fn execute(&self, op: FileOperation) -> Result<()>;
}

/// 文件操作计划。
#[derive(Debug, Clone)]
pub enum FileOperation {
    Move {
        sources: Vec<PathBuf>,
        target_dir: PathBuf,
    },
    Copy {
        sources: Vec<PathBuf>,
        target_dir: PathBuf,
    },
    Delete {
        sources: Vec<PathBuf>,
        permanent: bool,
    },
    Rename {
        source: PathBuf,
        new_name: String,
    },
}

/// 桌面层适配等级（规划 §9.5）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesktopLayerGrade {
    /// 普通无边框桌面窗口，任何系统版本均可退回。
    Safe,
    /// 通过公开 Shell 接口协作，管理图标显示状态。
    ShellCooperation,
    /// 仅在兼容测试通过后启用的嵌入层。
    EnhancedEmbed,
}

/// 桌面图标状态快照：接管前必须保存，退出/异常时还原。
pub trait DesktopIconState: Send {
    fn save(&self) -> Result<()>;
    fn restore(&self) -> Result<()>;
    fn grade(&self) -> DesktopLayerGrade;
}
