//! 领域错误类型。

use std::fmt;

/// ZSDesk 领域错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// 路径不存在或无法解析。
    NotFound(String),
    /// 无权限访问。
    AccessDenied(String),
    /// 设备断开或不可用（网络盘、移动盘拔出）。
    DeviceUnavailable(String),
    /// 输入参数无效。
    InvalidInput(String),
    /// 操作冲突（如目标目录是自身的子目录）。
    Conflict(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotFound(p) => write!(f, "路径不存在：{p}"),
            Error::AccessDenied(p) => write!(f, "无权限访问：{p}"),
            Error::DeviceUnavailable(p) => write!(f, "设备不可用：{p}"),
            Error::InvalidInput(msg) => write!(f, "无效输入：{msg}"),
            Error::Conflict(msg) => write!(f, "操作冲突：{msg}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;
