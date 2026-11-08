use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::{Mutex, atomic::{AtomicU64, Ordering}},
};

use anyhow::{Context, Result};
use zsdesk_core::model::explorer::ExplorerSession;

// A process may host several window threads. Serialize their read/merge/write
// transactions, not only the final rename, and never share a temporary file.
static SAVE_LOCK: Mutex<()> = Mutex::new(());
static SAVE_SERIAL: AtomicU64 = AtomicU64::new(1);

pub fn config_path() -> PathBuf {
    if let Ok(exe)=std::env::current_exe() {
        if let Some(dir)=exe.parent() {
            if dir.join("portable.flag").exists() { return dir.join("data").join("explorer-session.json"); }
        }
    }
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(crate::edition::profile_name())
        .join("explorer-session.json")
}

pub fn load(path: &Path) -> Result<ExplorerSession> {
    let text =
        fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    serde_json::from_str(&text).context("invalid explorer session")
}

pub fn default_session() -> ExplorerSession {
    let mut session = ExplorerSession::default();
    if crate::edition::THEMED {
        session.ui.appearance.backdrop = zsdesk_core::model::explorer::BackdropStyle::Acrylic;
    }
    session
}

pub fn save(path: &Path, session: &ExplorerSession) -> Result<()> {
    let _write = SAVE_LOCK.lock().map_err(|_| anyhow::anyhow!("会话保存锁异常"))?;
    save_unlocked(path, session)
}

/// A secondary window owns its appearance and bookmarks, while the primary
/// window remains the owner of the persisted tab session.
pub fn save_window(path: &Path, session: &ExplorerSession, secondary: bool) -> Result<()> {
    let _write = SAVE_LOCK.lock().map_err(|_| anyhow::anyhow!("会话保存锁异常"))?;
    if secondary {
        let mut saved = load(path)?;
        saved.ui = session.ui.clone();
        saved.bookmarks = session.bookmarks.clone();
        save_unlocked(path, &saved)
    } else {
        save_unlocked(path, session)
    }
}

fn save_unlocked(path: &Path, session: &ExplorerSession) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let temporary = path.with_extension(format!("{}-{}.json.tmp",std::process::id(),SAVE_SERIAL.fetch_add(1,Ordering::Relaxed)));
    let data = serde_json::to_vec_pretty(session)?;
    if let Err(error) = fs::write(&temporary, data).and_then(|_|replace_file(&temporary, path)) {
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::{Arc,Barrier},thread};
    use zsdesk_core::model::explorer::{ExplorerTab,ShellLocation};

    #[test]
    fn concurrent_secondary_saves_preserve_primary_tabs_and_leave_no_pending_files() {
        let root=std::env::temp_dir().join(format!("zsdesk-session-{}-{}",std::process::id(),SAVE_SERIAL.fetch_add(1,Ordering::Relaxed)));
        fs::create_dir(&root).unwrap();
        struct Fixture(PathBuf);
        impl Drop for Fixture {
            fn drop(&mut self){
                if let Ok(items)=fs::read_dir(&self.0){for item in items.flatten(){if item.file_type().is_ok_and(|kind|kind.is_file()){let _=fs::remove_file(item.path());}}}
                let _=fs::remove_dir(&self.0);
            }
        }
        let _fixture=Fixture(root.clone());let path=root.join("explorer-session.json");
        let mut primary=default_session();
        primary.tabs=vec![ExplorerTab::new(1,ShellLocation::ThisPc),ExplorerTab::new(2,ShellLocation::Path(root.clone()))];
        primary.active_tab=1;primary.next_id=3;save(&path,&primary).unwrap();
        let barrier=Arc::new(Barrier::new(9));let mut threads=Vec::new();
        for index in 0..8 {
            let mut secondary=primary.clone();secondary.tabs=vec![ExplorerTab::new(100+index,ShellLocation::ThisPc)];
            secondary.active_tab=0;secondary.ui.tree_width=180+index as i32;
            let path=path.clone();let barrier=barrier.clone();
            threads.push(thread::spawn(move||{barrier.wait();for _ in 0..12{save_window(&path,&secondary,true).unwrap();}}));
        }
        barrier.wait();
        for _ in 0..12{save_window(&path,&primary,false).unwrap();}
        for thread in threads{thread.join().unwrap();}
        let actual=load(&path).unwrap();assert_eq!(actual.tabs,primary.tabs);assert_eq!(actual.active_tab,1);assert_eq!(actual.next_id,3);
        assert_eq!(fs::read_dir(&root).unwrap().count(),1);
    }
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
