//! Build edition, selected with Cargo features (see scripts/package.ps1).
//!
//! | 版本 | features |
//! |---|---|
//! | 标准版 | （无） |
//! | 网络版 | `network`：FTP、SFTP/SSH、SMB 地址与网络驱动器 |
//! | 桌面版 | `desktop`：桌面格子 |
//! | 全量版 | `network` + `desktop`（默认） |

pub const NETWORK: bool = cfg!(feature = "network");
pub const DESKTOP: bool = cfg!(feature = "desktop");

pub const fn key() -> &'static str {
    match (NETWORK, DESKTOP) {
        (true, true) => "full",
        (true, false) => "network",
        (false, true) => "desktop",
        (false, false) => "standard",
    }
}

pub const fn name() -> &'static str {
    match (NETWORK, DESKTOP) {
        (true, true) => "全量版",
        (true, false) => "网络版",
        (false, true) => "桌面版",
        (false, false) => "标准版",
    }
}
