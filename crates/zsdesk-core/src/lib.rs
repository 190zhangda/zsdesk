//! ZSDesk 领域模型与公共类型。
//!
//! 分层原则：领域层不依赖 HWND、COM 指针或 ZSUI 控件，便于单元测试。

pub mod error;
pub mod model;
pub mod path;
