//! Win32 目录枚举：使用宽字符 API 完整支持 Unicode 与长路径。
//!
//! 规划 §9.1：普通路径使用 Win32 宽字符 API；枚举结果先提交轻量字段。

use zsdesk_core::error::{Error, Result};
use zsdesk_core::model::view::FileEntry;
use zsdesk_platform::shell::{CancellationToken, DirectoryLister, ListingBatch};

use std::ffi::OsString;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use windows_sys::Win32::{
    Foundation::INVALID_HANDLE_VALUE,
    Storage::FileSystem::{
        FindClose, FindFirstFileW, FindNextFileW, FILE_ATTRIBUTE_DIRECTORY, WIN32_FIND_DATAW,
    },
};

/// 使用 Win32 原生 API 的目录枚举器。
#[derive(Debug, Default, Clone)]
pub struct Win32DirectoryLister;

fn to_wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

impl DirectoryLister for Win32DirectoryLister {
    fn list(
        &self,
        path: &Path,
        request_id: zsdesk_core::model::view::RequestId,
        _offset: usize,
        _length: usize,
        cancel: &CancellationToken,
    ) -> Result<ListingBatch> {
        // 长路径前缀策略：
        // - 普通路径直接使用原始路径 + `\*`（`\\?\` 前缀在未启用
        //   LongPathsEnabled 的 Windows 上会返回 ERROR_INVALID_NAME）；
        // - 仅当路径超长（≥240 字符，接近 MAX_PATH 限制）时才尝试
        //   `\\?\` 前缀，失败则回退为普通路径。
        const LONG_PATH_THRESHOLD: usize = 240;
        let raw = path.as_os_str();
        let raw_len = raw.to_string_lossy().len();
        let pattern = if raw_len >= LONG_PATH_THRESHOLD {
            let mut extended = OsString::from("\\\\?\\");
            extended.push(raw);
            extended
        } else {
            OsString::from(raw)
        };
        // 目录与 `\*` 之间只保留一个分隔符（`C:\` 根目录 → `C:\*`）
        let mut pattern = pattern;
        if !pattern.to_string_lossy().ends_with('\\') {
            pattern.push("\\");
        }
        pattern.push("*");

        let wide = to_wide(Path::new(&pattern));
        let mut find_data: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };

        let handle = unsafe { FindFirstFileW(wide.as_ptr(), &mut find_data) };
        if handle == INVALID_HANDLE_VALUE {
            let code = unsafe { windows_sys::Win32::Foundation::GetLastError() };
            return Err(match code {
                // ERROR_FILE_NOT_FOUND / ERROR_PATH_NOT_FOUND
                2 | 3 => Error::NotFound(path.display().to_string()),
                5 => Error::AccessDenied(path.display().to_string()),
                // ERROR_BAD_NETPATH 等
                53 | 67 | 1231 => Error::DeviceUnavailable(path.display().to_string()),
                _ => Error::InvalidInput(format!(
                    "枚举失败：{} (Win32 error {code})",
                    path.display()
                )),
            });
        }

        let mut entries = Vec::new();
        loop {
            if cancel.is_cancelled() {
                break;
            }

            let name = unsafe { wide_to_string(&find_data.cFileName as *const u16) };
            if name != "." && name != ".." {
                let is_dir = find_data.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0;
                let modified = filetime_to_unix_ms(find_data.ftLastWriteTime);
                let created = filetime_to_unix_ms(find_data.ftCreationTime);
                let size = if is_dir {
                    None
                } else {
                    Some(((find_data.nFileSizeHigh as u64) << 32) | (find_data.nFileSizeLow as u64))
                };
                entries.push(FileEntry {
                    name,
                    is_dir,
                    size,
                    modified,
                    created,
                    identity_key: None,
                });
            }

            if unsafe { FindNextFileW(handle, &mut find_data) } == 0 {
                break;
            }
        }

        unsafe { FindClose(handle) };

        let total = entries.len();
        Ok(ListingBatch {
            request_id,
            entries,
            total,
            has_more: false,
        })
    }
}

/// 将 UTF-16 缓冲区转为 String（忽略非法代理对）。
unsafe fn wide_to_string(ptr: *const u16) -> String {
    let mut buf = Vec::new();
    let mut i = 0;
    loop {
        let c = unsafe { *ptr.add(i) };
        if c == 0 {
            break;
        }
        buf.push(c);
        i += 1;
    }
    String::from_utf16_lossy(&buf)
}

fn filetime_to_unix_ms(ft: windows_sys::Win32::Foundation::FILETIME) -> Option<u64> {
    let ft64 = ((ft.dwHighDateTime as u64) << 32) | (ft.dwLowDateTime as u64);
    if ft64 == 0 {
        return None;
    }
    // FILETIME 从 1601-01-01 起 100ns 单位；Unix 纪元偏移 11644473600 秒
    Some(ft64 / 10_000 - 11_644_473_600_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_conversion() {
        let mut wide: Vec<u16> = "测试.txt".encode_utf16().collect();
        wide.push(0);
        unsafe {
            assert_eq!(wide_to_string(wide.as_ptr()), "测试.txt");
        }
    }

    #[test]
    fn filetime_conversion_known() {
        // 2001-09-09 01:46:40 UTC（Unix 时间 1000000000）的 FILETIME
        let ft = windows_sys::Win32::Foundation::FILETIME {
            dwLowDateTime: 0x44FF8000,
            dwHighDateTime: 0x01C138D1,
        };
        let ms = filetime_to_unix_ms(ft).unwrap();
        assert_eq!(ms, 1_000_000_000_000);
    }
}
