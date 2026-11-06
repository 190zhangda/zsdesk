use std::{
    fs, io,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use zsdesk_core::model::explorer::ExplorerSession;

pub fn config_path() -> PathBuf {
    if let Ok(exe)=std::env::current_exe() {
        if let Some(dir)=exe.parent() {
            if dir.join("portable.flag").exists() { return dir.join("data").join("explorer-session.json"); }
        }
    }
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("ZSDesk")
        .join("explorer-session.json")
}

pub fn load(path: &Path) -> Result<ExplorerSession> {
    let text =
        fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    serde_json::from_str(&text).context("invalid explorer session")
}

pub fn save(path: &Path, session: &ExplorerSession) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let temporary = path.with_extension("json.tmp");
    let data = serde_json::to_vec_pretty(session)?;
    fs::write(&temporary, data)?;
    replace_file(&temporary, path)?;
    Ok(())
}

fn replace_file(temporary: &Path, destination: &Path) -> io::Result<()> {
    #[cfg(windows)] {
        use std::os::windows::ffi::OsStrExt;
        use windows::{core::PCWSTR,Win32::Storage::FileSystem::{MoveFileExW,MOVEFILE_REPLACE_EXISTING,MOVEFILE_WRITE_THROUGH}};
        let source:Vec<u16>=temporary.as_os_str().encode_wide().chain(Some(0)).collect();
        let target:Vec<u16>=destination.as_os_str().encode_wide().chain(Some(0)).collect();
        unsafe {MoveFileExW(PCWSTR(source.as_ptr()),PCWSTR(target.as_ptr()),MOVEFILE_REPLACE_EXISTING|MOVEFILE_WRITE_THROUGH)}
            .map_err(|e|io::Error::other(e.to_string()))
    }
    #[cfg(not(windows))] { fs::rename(temporary,destination) }
}
