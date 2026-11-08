//! Persistent filename search with NTFS metadata acceleration and directory coverage.
//! All reads/index maintenance run on one bounded worker; file contents are never read.

mod ntfs;
mod metadata;
pub(crate) mod everything;
pub use zsdesk_core::model::explorer::SearchBackendPreference as BackendPreference;

#[derive(Clone, Copy, Debug, Default)]
pub struct SearchOptions { pub backend: BackendPreference }

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all="snake_case")]
pub enum MaintenanceRole { Pending, Writer, Reader }

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all="snake_case")]
pub enum BackendKind { NativeNtfs, #[default] DirectoryScan, Mixed, Everything }
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all="snake_case")]
pub enum IndexPhase { #[default] Starting, Snapshot, CatchingUp, Scanning, Ready, Paused }
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct VolumeStatus {
    pub root: PathBuf, pub backend: BackendKind, pub phase: IndexPhase,
    pub indexed: u64, pub checkpoint: Option<i64>, pub warning: Option<String>,
}

use std::{collections::HashSet, ffi::{OsStr, OsString}, fs::{self, ReadDir},
    os::windows::{ffi::{OsStrExt, OsStringExt}, fs::MetadataExt}, path::{Component, Path, PathBuf, Prefix},
    sync::{Arc, Condvar, Mutex, atomic::{AtomicBool, AtomicU64, Ordering}, mpsc::{self, Receiver, SyncSender, TrySendError}},
    thread::{self, JoinHandle}, time::{Duration, Instant}};
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use windows::{core::PCWSTR, Win32::{Foundation::{HWND, LPARAM, WPARAM},
    Storage::FileSystem::{GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDriveStringsW},
    System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN},
    UI::WindowsAndMessaging::PostMessageW}};

const PAGE_CACHE_KIB: i64 = 2048;
const MAX_DATABASE_PAGES: i64 = 262_144; // 1 GiB at the fixed 4096-byte page size.
const MAX_WAL_BYTES: u64 = 32 * 1024 * 1024;
const MIN_FREE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_RESULTS: usize = 2000;
const RESULT_MEMORY_BYTES: usize = 2 * 1024 * 1024;
const EVENT_BATCH: usize = 64;
const EVENT_QUEUE: usize = 4;
const SCAN_BATCH: usize = 128;
const QUERY_ID_WINDOW: i64 = 4096;
const REPARSE_POINT: u32 = 0x400;
const APPLICATION_ID: i64 = 0x5A534653;

/// Nonempty `paths` are new results, never a replacement snapshot; each batch has at most 64 paths.
#[derive(Clone, Debug)]
pub struct SearchEvent {
    pub generation: u64,
    pub paths: Vec<PathBuf>,
    pub scanned: u64,
    pub indexed: u64,
    pub denied: u64,
    pub skipped_reparse: u64,
    pub scanning: bool,
    /// This query is complete, truncated, or ended with an explicit error. Indexing may continue.
    pub finished: bool,
    pub truncated: bool,
    pub error: Option<String>,
    /// Clear this generation's prior paths before applying this batch. Used when
    /// rename/delete or a journal reset changes already displayed results.
    pub reset: bool,
    pub backend: BackendKind,
    pub phase: IndexPhase,
    pub volumes: Vec<VolumeStatus>,
    /// Another process owns index maintenance. This query has its own lifetime.
    pub read_only: bool,
}

#[derive(Clone)]
struct Request { generation: u64, query: String, preferred: Option<PathBuf>, invalid: Option<String>,
    offset:u32, limit:u32, sort:everything::SearchSort }
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum ScanAction { #[default] None, Refresh, Rebuild }
#[derive(Default)]
struct Control { request: Option<Request>, action: ScanAction, release_idle:bool }
struct Shared {
    control: Mutex<Control>, wake: Condvar, stop: AtomicBool, generation: AtomicU64,
    notified: AtomicBool, hwnd: isize, message: u32,
    notifier: Option<Arc<dyn Fn() + Send + Sync>>,
    maintenance_role: AtomicU64,
    #[cfg(test)] trace_stage:AtomicU64,
}

pub struct FastSearch { shared: Arc<Shared>, events: Receiver<SearchEvent>, worker: Option<JoinHandle<()>> }

impl FastSearch {
    /// Construction does not scan disks or open the index; the first `start` does so on the worker.
    pub fn new(index_path: PathBuf, hwnd: HWND, message_id: u32) -> Result<Self> {
        Self::new_with_options(index_path, hwnd, message_id, SearchOptions::default())
    }

    /// The maintenance owner applies the backend preference. Other processes
    /// query its committed shared index and report their read-only role.
    pub fn new_with_options(index_path: PathBuf, hwnd: HWND, message_id: u32, options: SearchOptions) -> Result<Self> {
        Self::with_options_and_roots(index_path, hwnd, message_id, None, options, None)
    }

    pub fn new_with_notifier(index_path: PathBuf, options: SearchOptions, notify: Arc<dyn Fn() + Send + Sync>) -> Result<Self> {
        Self::with_options_and_roots(index_path, HWND::default(), 0, None, options, Some(notify))
    }

    #[cfg(test)]
    pub(crate) fn new_with_roots_for_test(index_path: PathBuf, roots: Vec<PathBuf>) -> Result<Self> {
        Self::with_roots(index_path, HWND::default(), 0, Some(roots))
    }

    #[cfg(test)]
    fn with_roots(index_path: PathBuf, hwnd: HWND, message_id: u32, roots: Option<Vec<PathBuf>>) -> Result<Self> {
        Self::with_options_and_roots(index_path, hwnd, message_id, roots, SearchOptions { backend: BackendPreference::DirectoryScan }, None)
    }

    fn with_options_and_roots(index_path: PathBuf, hwnd: HWND, message_id: u32, roots: Option<Vec<PathBuf>>, options: SearchOptions,
        notifier: Option<Arc<dyn Fn() + Send + Sync>>) -> Result<Self> {
        let index_path = std::path::absolute(index_path)?;
        let shared = Arc::new(Shared { control: Mutex::new(Control::default()), wake: Condvar::new(),
            stop: AtomicBool::new(false), generation: AtomicU64::new(0), notified: AtomicBool::new(false),
            hwnd: hwnd.0 as isize, message: message_id, notifier, maintenance_role: AtomicU64::new(0),#[cfg(test)] trace_stage:AtomicU64::new(0) });
        let (sender, events) = mpsc::sync_channel(EVENT_QUEUE);
        let state = shared.clone();
        let worker = thread::Builder::new().name("zsdesk-file-index".into())
            .spawn(move || worker_loop(index_path, roots, state, sender, options)).context("无法启动文件名索引线程")?;
        Ok(Self { shared, events, worker: Some(worker) })
    }

    /// Replaces an earlier request immediately. Empty input cancels the query.
    pub fn start(&self, query: &str, preferred: Option<PathBuf>) -> u64 {
        self.start_page(query,preferred,0,2000,everything::SearchSort::NameAsc)
    }

    pub fn start_page(&self, query:&str, preferred:Option<PathBuf>, offset:u32, limit:u32, sort:everything::SearchSort)->u64 {
        let generation = self.shared.generation.fetch_add(1, Ordering::AcqRel) + 1;
        self.discard_events();
        let text: String = query.trim().chars().take(513).collect();
        let invalid = if text.chars().count() > 512 { Some("搜索内容过长，请缩短文件名关键词。".into()) }
            else if text.contains('\0') { Some("搜索内容包含无效字符。".into()) } else { None };
        let request = (!text.is_empty()).then(|| Request { generation, query: text, preferred, invalid, offset, limit:limit.clamp(1,2000), sort });
        {let mut control=self.shared.control.lock().unwrap_or_else(|e|e.into_inner());control.release_idle=request.is_none();control.request=request;}
        self.shared.wake.notify_one();
        generation
    }

    /// Cancels the query and releases idle index maintenance; committed data remains reusable.
    pub fn cancel(&self) {
        self.shared.generation.fetch_add(1, Ordering::AcqRel);
        {let mut control=self.shared.control.lock().unwrap_or_else(|e|e.into_inner());control.request=None;control.action=ScanAction::None;control.release_idle=true;}
        self.discard_events();
        self.shared.wake.notify_one();
    }

    /// Retains searchable records and schedules a new file-system pass. Call `start` for the new view.
    pub fn refresh(&self) { self.schedule_scan(ScanAction::Refresh); }

    /// Clears only the owned index records and rebuilds them. Never removes source files.
    pub fn rebuild(&self)->u64 { self.schedule_scan(ScanAction::Rebuild);self.shared.generation.load(Ordering::Acquire) }

    /// Maintenance ownership exists independently of any active query/events.
    pub fn maintenance_role(&self) -> MaintenanceRole {
        match self.shared.maintenance_role.load(Ordering::Acquire) {
            1 => MaintenanceRole::Writer, 2 => MaintenanceRole::Reader, _ => MaintenanceRole::Pending,
        }
    }

    fn schedule_scan(&self, action: ScanAction) {
        self.shared.generation.fetch_add(1, Ordering::AcqRel);
        let mut control = self.shared.control.lock().unwrap_or_else(|e| e.into_inner());
        control.request = None;
        control.release_idle=false;
        if control.action != ScanAction::Rebuild { control.action = action; }
        drop(control);
        self.discard_events();
        self.shared.wake.notify_one();
    }

    /// Bounded, nonblocking drain. Stale generations are discarded before reaching the UI.
    pub fn poll(&self) -> Vec<SearchEvent> {
        self.shared.notified.store(false, Ordering::Release);
        let generation = self.shared.generation.load(Ordering::Acquire);
        let mut result = Vec::with_capacity(EVENT_QUEUE);
        let mut drained = 0;
        for _ in 0..EVENT_QUEUE {
            let Ok(event) = self.events.try_recv() else { break };
            drained += 1;
            if event.generation == generation { result.push(event); }
        }
        if drained > 0 { self.shared.wake.notify_one(); }
        if drained == EVENT_QUEUE { self.shared.notify(); }
        result
    }

    fn discard_events(&self) {
        self.shared.notified.store(false, Ordering::Release);
        for _ in 0..EVENT_QUEUE { if self.events.try_recv().is_err() { break; } }
    }
}

impl Drop for FastSearch {
    fn drop(&mut self) {
        let control = self.shared.control.lock().unwrap_or_else(|e| e.into_inner());
        self.shared.stop.store(true, Ordering::Release);
        drop(control);
        self.shared.wake.notify_all();
        // Local filesystem calls themselves are not cancellable. A slow call must
        // not block the UI; the owned worker closes its connection when that call returns.
        if let Some(worker) = self.worker.take() {
            if worker.is_finished() { let _ = worker.join(); }
        }
    }
}

impl Shared {
    fn trace(&self,_stage:u64){#[cfg(test)] self.trace_stage.store(_stage,Ordering::Relaxed);}
    fn notify(&self) {
        if self.stop.load(Ordering::Acquire) || (self.notifier.is_none() && (self.hwnd == 0 || self.message == 0)) || self.notified.swap(true, Ordering::AcqRel) { return; }
        if let Some(notify) = &self.notifier { notify(); return; }
        if unsafe { PostMessageW(Some(HWND(self.hwnd as *mut _)), self.message, WPARAM(0), LPARAM(0)) }.is_err() {
            self.notified.store(false, Ordering::Release);
        }
    }
    fn current(&self, generation: u64) -> bool {
        !self.stop.load(Ordering::Acquire) && self.generation.load(Ordering::Acquire) == generation
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct Stats { scanned: u64, indexed: u64, denied: u64, skipped_reparse: u64, scanning: bool }
#[derive(Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct IndexView { backend: BackendKind, phase: IndexPhase, volumes: Vec<VolumeStatus>, read_only: bool }
#[derive(Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct Progress { stats: Stats, view: IndexView, #[serde(default)] revision: u64 }
impl Progress {
    fn writer(stats: Stats, native: Option<&metadata::Engine>, paused: bool, repair: bool, revision: u64) -> Self {
        let mut combined = stats;
        if let Some(native) = native {
            // These are index records, not a deduplicated physical-file count:
            // directory coverage and the FRN graph intentionally overlap.
            combined.indexed = combined.indexed.saturating_add(native.indexed());
            combined.scanned = combined.scanned.saturating_add(native.scanned);
            combined.scanning |= native.busy();
        }
        combined.scanning = !paused && (combined.scanning || repair);
        Self { stats: combined, revision, view: IndexView {
            backend: native.map_or(BackendKind::DirectoryScan, metadata::Engine::backend),
            phase: if paused { IndexPhase::Paused } else { native.map_or(if combined.scanning { IndexPhase::Scanning } else { IndexPhase::Ready }, |n| n.stage(stats.scanning || repair)) },
            volumes: native.map_or_else(Vec::new, |n| n.statuses(stats.scanning || repair)), read_only: false,
        } }
    }
}
struct Database { connection: Connection, path: PathBuf, stats: Stats, revision: u64, epoch: i64, batches: u64,
    read_only: bool, result_repair: bool, cancellation: Option<Arc<Shared>> }

struct IndexOwner { handle: Option<windows::Win32::Foundation::HANDLE>, name: Vec<u16>, writer: bool }
impl IndexOwner {
    fn new(path: &Path) -> Result<Self> {
        use sha2::{Digest, Sha256};
        let canonical = fs::canonicalize(path).unwrap_or_else(|_| {
            path.parent().and_then(|p| fs::canonicalize(p).ok()).map(|p| p.join(path.file_name().unwrap_or_default())).unwrap_or_else(|| path.to_path_buf())
        });
        let identity = format!("{:x}", Sha256::digest(path_key(&canonical).to_lowercase().as_bytes()));
        // Global mutexes serialize the same profile across desktop/RDP sessions;
        // unlike global file mappings, mutex creation needs no added privilege.
        let name: Vec<u16> = format!("Global\\ZSDesk.Search.Writer.{identity}").encode_utf16().chain(Some(0)).collect();
        let mut result = Self { handle: None, name, writer: false };
        result.open()?; result.acquire(); Ok(result)
    }
    fn open(&mut self) -> Result<()> {
        use windows::Win32::System::Threading::CreateMutexW;
        match unsafe { CreateMutexW(None, false, PCWSTR(self.name.as_ptr())) } {
            Ok(handle) => self.handle = Some(handle),
            // An inaccessible/elevated maintainer does not authorize a second
            // writer. Read-only SQLite access may still be available. Retry the
            // kernel object later without changing its ACL or the process token.
            Err(error) if error.code().0 as u32 == 0x80070005 => {},
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }
    fn acquire(&mut self) -> bool {
        use windows::Win32::{Foundation::{WAIT_ABANDONED, WAIT_OBJECT_0}, System::Threading::WaitForSingleObject};
        if !self.writer {
            if self.handle.is_none() && self.open().is_err() { return false; }
            let Some(handle) = self.handle else { return false };
            let state = unsafe { WaitForSingleObject(handle, 0) };
            self.writer = state == WAIT_OBJECT_0 || state == WAIT_ABANDONED;
        }
        self.writer
    }
}
impl Drop for IndexOwner {
    fn drop(&mut self) {
        if let Some(handle) = self.handle {
            if self.writer { let _ = unsafe { windows::Win32::System::Threading::ReleaseMutex(handle) }; }
            let _ = unsafe { windows::Win32::Foundation::CloseHandle(handle) };
        }
    }
}

impl Database {
    fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() { fs::create_dir_all(parent).context("无法创建索引目录")?; }
        let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE | OpenFlags::SQLITE_OPEN_NO_MUTEX)
            .context("无法打开文件名索引")?;
        connection.busy_timeout(Duration::from_millis(250))?;
        let application: i64 = connection.query_row("PRAGMA application_id", [], |r| r.get(0))?;
        let tables: i64 = connection.query_row("SELECT count(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'", [], |r| r.get(0))?;
        if application != APPLICATION_ID && (application != 0 || tables > 0) { bail!("索引文件不是可识别的 ZSDesk 文件名索引"); }
        let version: i64 = connection.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version > 2 { bail!("文件名索引由较新版本创建"); }
        connection.execute_batch(&format!("PRAGMA page_size=4096; PRAGMA cache_size=-{PAGE_CACHE_KIB}; PRAGMA mmap_size=0;
            PRAGMA temp_store=FILE; PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;
            PRAGMA wal_autocheckpoint=256; PRAGMA journal_size_limit=4194304; PRAGMA max_page_count={MAX_DATABASE_PAGES};
            PRAGMA application_id={APPLICATION_ID};"))?;
        connection.execute_batch("CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY,value INTEGER NOT NULL) WITHOUT ROWID;
            CREATE TABLE IF NOT EXISTS directories(id INTEGER PRIMARY KEY,path BLOB NOT NULL,path_key TEXT NOT NULL UNIQUE,
                root INTEGER NOT NULL,priority INTEGER NOT NULL,pending INTEGER NOT NULL DEFAULT 0,completed INTEGER NOT NULL DEFAULT 0);
            CREATE INDEX IF NOT EXISTS directories_pending ON directories(pending,priority,id);
            CREATE TABLE IF NOT EXISTS entries(id INTEGER PRIMARY KEY,directory_id INTEGER NOT NULL,name BLOB NOT NULL,
                folded TEXT NOT NULL,is_dir INTEGER NOT NULL,reparse INTEGER NOT NULL,seen INTEGER NOT NULL,
                UNIQUE(directory_id,name));")?;
        metadata::schema(&connection)?;
        connection.execute_batch("CREATE TABLE IF NOT EXISTS search_state(key TEXT PRIMARY KEY,value TEXT NOT NULL) WITHOUT ROWID;")?;
        connection.execute_batch("PRAGMA user_version=2;")?;
        let indexed = connection.query_row("SELECT count(*) FROM entries", [], |r| r.get::<_, i64>(0))? as u64;
        let epoch = meta(&connection, "epoch")?;
        let revision = meta(&connection, "query_revision")?.max(0) as u64;
        Ok(Self { connection, path: path.to_path_buf(), stats: Stats { indexed, ..Default::default() }, revision, epoch,
            batches: 0, read_only: false, result_repair: false, cancellation: None })
    }

    fn open_read_only(path: &Path) -> Result<Self> {
        let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
        connection.busy_timeout(Duration::from_millis(250))?;
        let application: i64 = connection.query_row("PRAGMA application_id", [], |r| r.get(0))?;
        let version: i64 = connection.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version > 2 { bail!("文件名索引由较新版本创建"); }
        if application != APPLICATION_ID || version != 2 { bail!("另一个进程尚未完成索引初始化"); }
        connection.execute_batch(&format!("PRAGMA query_only=ON; PRAGMA cache_size=-{PAGE_CACHE_KIB}; PRAGMA mmap_size=0; PRAGMA temp_store=FILE;"))?;
        let indexed = connection.query_row("SELECT count(*) FROM entries", [], |r| r.get::<_,i64>(0))?.max(0) as u64;
        let scanning = meta(&connection, "scan_active")? == 1;
        let epoch = meta(&connection, "epoch")?;
        Ok(Self { connection, path: path.to_path_buf(), stats: Stats { indexed, scanning, ..Default::default() }, revision: 0, epoch,
            batches: 0, read_only: true, result_repair: false, cancellation: None })
    }

    fn start_scan(&mut self, roots: &[PathBuf], preferred: Option<&Path>, force: bool, clear: bool) -> Result<()> {
        if self.read_only { bail!("此窗口只读查询，索引由另一进程维护"); }
        if clear { self.clear_owned_records()?; }
        let transaction = self.connection.transaction()?;
        let mut stats = self.stats;
        if clear {
            stats.indexed = 0;
        }
        let resume = !force && !clear && meta(&transaction, "directory_repair")? == 0
            && meta(&transaction, "scan_active")? == 1 && self.epoch > 0;
        let mut epoch = self.epoch;
        if !resume {
            epoch = epoch.checked_add(1).filter(|n| *n > 0).ok_or_else(|| anyhow::anyhow!("索引扫描代次超出范围"))?;
            stats.scanned = 0; stats.denied = 0; stats.skipped_reparse = 0;
            set_meta(&transaction, "epoch", epoch)?;
            for (root, path) in roots.iter().enumerate() { queue_directory(&transaction, path, root, 10 + root as i64, epoch)?; }
            set_meta(&transaction, "directory_repair", 0)?;
        } else {
            stats.scanned = meta(&transaction, "scanned")?.max(0) as u64;
            stats.denied = meta(&transaction, "denied")?.max(0) as u64;
            stats.skipped_reparse = meta(&transaction, "reparse")?.max(0) as u64;
        }
        stats.scanning = true;
        set_meta(&transaction, "scan_active", 1)?;
        transaction.commit()?;
        self.epoch = epoch; self.stats = stats;
        if clear { self.revision += 1; }
        self.prioritize(roots, preferred)?;
        Ok(())
    }

    fn clear_owned_records(&mut self) -> Result<()> {
        if self.read_only { bail!("只读索引不能清空"); }
        // An interrupted rebuild restarts coverage, never resumes a half-cleared
        // directory epoch. Batches keep a rebuild from creating a GiB-sized WAL.
        set_meta(&self.connection, "scan_active", 1)?;
        set_meta(&self.connection, "directory_repair", 1)?;
        // Removing the volume checkpoint first forces a fresh snapshot after an
        // interrupted clear; an old READY cursor must not describe partial nodes.
        self.connection.execute("DELETE FROM ntfs_volumes", [])?;
        for (table, batch) in [("entries", 256), ("directories", 32), ("ntfs_nodes", 256)] {
            loop {
                if self.cancellation.as_ref().is_some_and(|s| s.stop.load(Ordering::Acquire)) { bail!("索引重建已取消"); }
                let removed = self.connection.execute(&format!("DELETE FROM {table} WHERE id IN (SELECT id FROM {table} LIMIT {batch})"), [])?;
                self.check_wal()?;
                if removed == 0 { break; }
            }
        }
        loop {
            if self.cancellation.as_ref().is_some_and(|s| s.stop.load(Ordering::Acquire)) { bail!("索引重建已取消"); }
            let removed = self.connection.execute("DELETE FROM ntfs_gc WHERE (volume,frn) IN (SELECT volume,frn FROM ntfs_gc LIMIT 256)", [])?;
            self.check_wal()?;
            if removed == 0 { break; }
        }
        Ok(())
    }

    fn queue_directory(&self, path: &Path, root: usize, priority: i64) -> Result<()> {
        queue_directory(&self.connection, path, root, priority, self.epoch)
    }

    fn prioritize(&self, roots: &[PathBuf], preferred: Option<&Path>) -> Result<()> {
        let Some(path) = preferred.filter(|p| p.is_absolute()) else { return Ok(()) };
        let Some(root) = containing_root(path, roots) else { return Ok(()) };
        // Check every ancestor before canonicalizing a user-supplied preferred path.
        if !ordinary_directory_chain(path).unwrap_or(false) { return Ok(()) }
        let canonical = plain_path(&fs::canonicalize(path)?);
        self.connection.execute("UPDATE directories SET priority=10+root WHERE pending=?1 AND priority<10", [self.epoch])?;
        self.connection.execute("UPDATE directories SET priority=min(priority,1) WHERE pending=?1 AND root=?2", params![self.epoch, root as i64])?;
        self.queue_directory(&roots[root], root, 1)?;
        self.queue_directory(&canonical, root, 0)?;
        Ok(())
    }

    fn next_directory(&self) -> Result<Option<DirectoryWork>> {
        let row: Option<(i64, Vec<u8>, i64, i64)> = self.connection.query_row(
            "SELECT id,path,root,priority FROM directories WHERE pending=?1 ORDER BY priority,id LIMIT 1", [self.epoch],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).optional()?;
        row.map(|(id, path, root, priority)| Ok(DirectoryWork { id, path: PathBuf::from(decode_os(&path)?), root: root as usize,
            priority, iterator: None, failed: false })).transpose()
    }

    fn finish_directory(&mut self, directory: &DirectoryWork) -> Result<()> {
        if !directory.failed {
            let removed = self.connection.execute("DELETE FROM entries WHERE directory_id=?1 AND seen<>?2", params![directory.id, self.epoch])?;
            self.stats.indexed = self.stats.indexed.saturating_sub(removed as u64);
            self.revision += removed as u64;
            self.result_repair |= removed > 0;
        }
        self.connection.execute("UPDATE directories SET pending=0,completed=?2 WHERE id=?1", params![directory.id, self.epoch])?;
        self.persist_progress()?;
        Ok(())
    }

    fn persist_progress(&self) -> Result<()> {
        if self.read_only { return Ok(()); }
        set_meta(&self.connection, "scanned", self.stats.scanned as i64)?;
        set_meta(&self.connection, "denied", self.stats.denied as i64)?;
        set_meta(&self.connection, "reparse", self.stats.skipped_reparse as i64)?;
        Ok(())
    }

    fn write_status(&mut self, progress: &Progress) -> Result<()> {
        if self.read_only { return Ok(()); }
        let transaction = self.connection.transaction()?;
        set_meta(&transaction, "query_revision", i64::try_from(progress.revision)?)?;
        transaction.execute("INSERT INTO search_state(key,value) VALUES('progress',?1)
            ON CONFLICT(key) DO UPDATE SET value=excluded.value", [serde_json::to_string(progress)?])?;
        transaction.commit()?;
        Ok(())
    }
    fn read_status(&self) -> Result<Progress> {
        let text: Option<String> = self.connection.query_row("SELECT value FROM search_state WHERE key='progress'", [], |r| r.get(0)).optional()?;
        let mut progress = text.filter(|s| s.len() <= 65536).map(|s| serde_json::from_str::<Progress>(&s)).transpose()?.unwrap_or_else(|| Progress {
            stats: self.stats, revision: 0, view: IndexView { phase: IndexPhase::Starting, ..Default::default() },
        });
        progress.view.read_only = true;
        if meta(&self.connection, "scan_active")? == 1 || meta(&self.connection, "directory_repair")? != 0 {
            progress.stats.scanning = true;
            if progress.view.phase == IndexPhase::Ready { progress.view.phase = IndexPhase::Scanning; }
        }
        Ok(progress)
    }

    fn checkpoint(&self) { if !self.read_only { let _ = self.connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);"); } }

    fn check_wal(&self) -> Result<()> {
        let mut name = self.path.as_os_str().to_os_string(); name.push("-wal");
        let path = PathBuf::from(name);
        if fs::metadata(&path).is_ok_and(|m| m.len() > MAX_WAL_BYTES) {
            self.checkpoint();
            if fs::metadata(&path).is_ok_and(|m| m.len() > MAX_WAL_BYTES) {
                bail!("索引日志暂时无法合并，已暂停更新以限制磁盘占用");
            }
        }
        Ok(())
    }
}

fn meta(connection: &Connection, key: &str) -> Result<i64> {
    // Before schema creation callers do not use this helper.
    Ok(connection.query_row("SELECT value FROM meta WHERE key=?1", [key], |r| r.get(0)).optional()?.unwrap_or(0))
}
fn set_meta(connection: &Connection, key: &str, value: i64) -> Result<()> {
    connection.execute("INSERT INTO meta(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![key, value])?;
    Ok(())
}
fn queue_directory(connection: &Connection, path: &Path, root: usize, priority: i64, epoch: i64) -> Result<()> {
    connection.execute("INSERT INTO directories(path,path_key,root,priority,pending) VALUES(?1,?2,?3,?4,?5)
        ON CONFLICT(path_key) DO UPDATE SET path=excluded.path,root=excluded.root,priority=min(directories.priority,excluded.priority),
        pending=CASE WHEN directories.completed<>?5 THEN ?5 ELSE directories.pending END",
        params![encode_os(path.as_os_str()), path_key(path), root as i64, priority, epoch])?;
    Ok(())
}

struct DirectoryWork { id: i64, path: PathBuf, root: usize, priority: i64, iterator: Option<ReadDir>, failed: bool }
struct ScannedEntry { name: OsString, folded: String, directory: bool, reparse: bool }
struct QueryWork {
    generation: u64, query: String, cursor: i64, max_id: i64, paths: Vec<PathBuf>, seen: HashSet<String>,
    metadata_cursor: i64, metadata_max_id: i64, reset: bool, read_only: bool,
    bytes: usize, delivered: usize, truncated: bool, final_sent: bool, error: Option<String>, last_progress: Instant,
    last_stats: Stats,
    last_view: IndexView,
}

impl QueryWork {
    fn new(request: &Request, database: &Database) -> Result<Self> {
        let max_id = database.connection.query_row("SELECT coalesce(max(id),0) FROM entries", [], |r| r.get(0))?;
        let metadata_max_id = database.connection.query_row("SELECT coalesce(max(id),0) FROM ntfs_nodes", [], |r| r.get(0))?;
        Ok(Self { generation: request.generation, query: request.query.to_lowercase(), cursor: 0, max_id, paths: Vec::new(),
            metadata_cursor: 0, metadata_max_id, reset: false, read_only: database.read_only,
            seen: HashSet::new(), bytes: 0, delivered: 0, truncated: false, final_sent: false,
            error: request.invalid.clone(), last_progress: Instant::now() - Duration::from_secs(1), last_stats: Stats::default(), last_view: IndexView::default() })
    }
    fn add(&mut self, path: PathBuf) {
        let key = path_key(&path);
        if self.seen.contains(&key) { return; }
        // Count retained allocations, with room for the vector/hash-table slots.
        let bytes = path.capacity().saturating_add(key.capacity()).saturating_add(96);
        if self.paths.len() >= MAX_RESULTS || self.bytes.saturating_add(bytes) > RESULT_MEMORY_BYTES { self.truncated = true; return; }
        self.bytes += bytes; self.seen.insert(key); self.paths.push(path);
    }
    fn pass_complete(&self) -> bool { self.cursor >= self.max_id && self.metadata_cursor >= self.metadata_max_id }
    fn done(&self, stats: Stats) -> bool { self.error.is_some() || self.truncated || (self.pass_complete() && (self.read_only || !stats.scanning)) }
    fn event(&self, progress: &Progress, paths: Vec<PathBuf>, finished: bool) -> SearchEvent {
        let stats = progress.stats;
        SearchEvent { generation: self.generation, paths, scanned: stats.scanned, indexed: stats.indexed,
            denied: stats.denied, skipped_reparse: stats.skipped_reparse, scanning: stats.scanning,
            finished, truncated: self.truncated, error: self.error.clone(), reset: self.reset,
            backend: progress.view.backend, phase: progress.view.phase,
            volumes: progress.view.volumes.clone(), read_only: self.read_only }
    }
}

struct CachedQuery { query: String, revision: u64, paths: Vec<PathBuf>, truncated: bool }
struct MaintenanceRoleGuard(Arc<Shared>);
impl Drop for MaintenanceRoleGuard {
    fn drop(&mut self) { self.0.maintenance_role.store(0, Ordering::Release); }
}

fn worker_loop(path: PathBuf, roots_override: Option<Vec<PathBuf>>, shared: Arc<Shared>, sender: SyncSender<SearchEvent>, options: SearchOptions) {
    let _role_guard = MaintenanceRoleGuard(shared.clone());
    let configured=crate::storage::load(&path.with_file_name("explorer-session.json")).ok().and_then(|session|session.ui.everything_executable);
    // Production queries always use Everything. The bounded directory engine
    // below is retained solely for explicit fixture-root regression tests.
    if roots_override.is_none(){
        everything::run(shared,sender,configured);return;
    }
    let _ = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN) };
    let mut database: Option<Database> = None;
    let mut roots = Vec::new();
    let mut current: Option<DirectoryWork> = None;
    let mut active: Option<QueryWork> = None;
    let mut cache: Option<CachedQuery> = None;
    let mut native: Option<metadata::Engine> = None;
    let mut last_request: Option<Request> = None;
    let mut repair_pending = false;
    let mut started = false;
    let mut paused = false;
    let mut last_preferred: Option<String> = None;
    let mut ownership: Option<IndexOwner> = None;
    let mut waiting_since: Option<Instant> = None;
    let mut needs_setup = false;
    let mut takeover_reset = false;
    let mut reader_version = -1i64;
    let mut reader_progress = Progress::default();
    let mut last_reader_poll = Instant::now() - Duration::from_secs(2);
    let mut last_status_write = Instant::now() - Duration::from_secs(1);
    let mut saved_progress: Option<Progress> = None;
    let mut busy_since: Option<Instant> = None;
    let mut last_drive_check = Instant::now();
    let mut maintenance_error:Option<String>=None;
    while !shared.stop.load(Ordering::Acquire) {
        let mut control = {
            let mut control = shared.control.lock().unwrap_or_else(|e| e.into_inner());
            std::mem::take(&mut *control)
        };
        if control.release_idle&&control.request.is_none()&&control.action==ScanAction::None{
            active=None;last_request=None;current=None;native=None;database=None;ownership=None;cache=None;
            started=false;paused=false;needs_setup=false;waiting_since=None;repair_pending=false;maintenance_error=None;
            shared.maintenance_role.store(0,Ordering::Release);
        }
        if ownership.as_mut().is_some_and(|owner| !owner.writer && owner.acquire()) {
            shared.maintenance_role.store(1, Ordering::Release);
            database = None; native = None; current = None; cache = None;
            started = false; paused = false; needs_setup = true; waiting_since = None; takeover_reset = true;
            if control.request.is_none() { control.request = last_request.as_ref().filter(|r| shared.current(r.generation)).cloned(); }
        }
        if waiting_since.is_some() && control.request.is_none() {
            control.request = last_request.as_ref().filter(|r| shared.current(r.generation)).cloned();
        }
        if started && !paused && roots_override.is_none() && last_drive_check.elapsed() >= Duration::from_secs(10) {
            last_drive_check = Instant::now();
            let updated = fixed_roots();
            if !updated.is_empty() && updated != roots {
                roots = updated; cache = None; takeover_reset = true;
                if control.action != ScanAction::Rebuild { control.action = ScanAction::Refresh; }
                if control.request.is_none() { control.request = last_request.as_ref().filter(|r| shared.current(r.generation)).cloned(); }
            }
        }
        if active.as_ref().is_some_and(|query| !shared.current(query.generation)) { active = None; }
        let mut did_work = false;
        if let Some(request) = control.request.as_ref().filter(|r| r.invalid.is_some()) {
            let event = SearchEvent { generation: request.generation, paths: Vec::new(), scanned: 0, indexed: 0, denied: 0,
                skipped_reparse: 0, scanning: database.as_ref().is_some_and(|db| db.stats.scanning), finished: true,
                truncated: false, error: request.invalid.clone(), reset: false, backend: BackendKind::DirectoryScan,
                phase: IndexPhase::Starting, volumes: Vec::new(), read_only: database.as_ref().is_some_and(|db| db.read_only) };
            let _ = sender.try_send(event); shared.notify();
            continue;
        }
        if control.request.is_some() || control.action != ScanAction::None || needs_setup {
            let setup = (|| -> Result<()> {
                if let Some(request) = &control.request { last_request = Some(request.clone()); }
                if database.is_none() {
                    roots = roots_override.clone().unwrap_or_else(fixed_roots);
                    if roots.is_empty() { bail!("没有可建立索引的本地固定磁盘"); }
                    if ownership.is_none() {
                        ownership = Some(IndexOwner::new(&path)?);
                        shared.maintenance_role.store(if ownership.as_ref().unwrap().writer {1} else {2}, Ordering::Release);
                    }
                    if ownership.as_ref().is_some_and(|owner| owner.writer) {
                        shared.trace(5);
                        database = Some(Database::open(&path)?);
                        shared.trace(6);
                    } else {
                        match Database::open_read_only(&path) {
                            Ok(db) => { database = Some(db); waiting_since = None; },
                            Err(error) => {
                                let since = waiting_since.get_or_insert_with(Instant::now);
                                if since.elapsed() >= Duration::from_secs(5) { waiting_since = None; return Err(error); }
                                needs_setup = true;
                                if let Some(request) = &control.request {
                                    let event = SearchEvent { generation: request.generation, paths: Vec::new(), scanned: 0, indexed: 0,
                                        denied: 0, skipped_reparse: 0, scanning: true, finished: false, truncated: false, error: None,
                                        reset: false, backend: BackendKind::DirectoryScan, phase: IndexPhase::Starting, volumes: Vec::new(), read_only: true };
                                    let _ = sender.try_send(event); shared.notify();
                                }
                                return Ok(());
                            }
                        }
                    }
                }
                let db = database.as_mut().unwrap();
                db.cancellation = Some(shared.clone());
                if db.read_only {
                    if control.action == ScanAction::Rebuild { bail!("另一进程正在维护此索引，当前只读窗口不能重建或清空它"); }
                    reader_version = db.connection.query_row("PRAGMA data_version", [], |r| r.get(0))?;
                    reader_progress = db.read_status()?;
                    last_reader_poll = Instant::now(); cache = None; started = true; paused = false;
                } else if control.action != ScanAction::None || !started {
                    let pages:i64=db.connection.query_row("PRAGMA page_count",[],|row|row.get(0))?;
                    let free:i64=db.connection.query_row("PRAGMA freelist_count",[],|row|row.get(0))?;
                    if control.action!=ScanAction::Rebuild && pages-free>=MAX_DATABASE_PAGES-32 {
                        started=true;paused=true;db.stats.scanning=false;
                        maintenance_error=Some("本机索引已达到 1 GiB 容量上限，维护已暂停；当前结果仅覆盖已收录文件，可在界面设置中选择 Everything。".into());
                    } else {
                    current = None; active = None; cache = None;
                    shared.trace(7);
                    db.start_scan(&roots, control.request.as_ref().and_then(|r| r.preferred.as_deref()),
                        control.action != ScanAction::None, control.action == ScanAction::Rebuild)?;
                    shared.trace(8);
                    native = Some(metadata::Engine::new(&mut db.connection, &roots, options.backend, &shared.stop)?);
                    shared.trace(9);
                    repair_pending = meta(&db.connection, "directory_repair")? != 0;
                    started = true; paused = false;
                    // A new maintainer/pass invalidates peer snapshots even when
                    // the prior writer exited before publishing its last status.
                    db.revision = db.revision.checked_add(1).ok_or_else(|| anyhow::anyhow!("索引修订号超出范围"))?;
                    let progress = Progress::writer(db.stats, native.as_ref(), false, repair_pending, db.revision);
                    db.write_status(&progress)?; saved_progress = Some(progress); last_status_write = Instant::now();
                    maintenance_error=None;
                    }
                }
                needs_setup = false;
                if let Some(request) = &control.request {
                    if !shared.current(request.generation) { return Ok(()); }
                    if !db.read_only && db.stats.scanning && request.preferred.as_ref().map(|p| path_key(p)) != last_preferred {
                        db.prioritize(&roots, request.preferred.as_deref())?;
                        // A preferred directory can preempt a large in-progress directory at a batch boundary.
                        if request.preferred.is_some() { current = None; }
                    }
                    last_preferred = request.preferred.as_ref().map(|p| path_key(p));
                    shared.trace(10);
                    let mut query = QueryWork::new(request, db)?;
                    if !db.read_only && options.backend == BackendPreference::DirectoryScan { query.metadata_max_id = 0; }
                    query.reset = takeover_reset; takeover_reset = false;
                    if let Some(cached) = cache.as_ref().filter(|c| c.query == request.query.to_lowercase() && c.revision == db.revision) {
                        for item in &cached.paths {
                            if !shared.current(request.generation) { break; }
                            if let Ok(Some(path)) = verified_result(item, &roots) { query.add(path); }
                        }
                        query.truncated = cached.truncated;
                        query.cursor = query.max_id;
                        query.metadata_cursor = query.metadata_max_id;
                    }
                    last_request = Some(request.clone());
                    active = Some(query);
                    shared.trace(11);
                }
                Ok(())
            })();
            if let Err(error) = setup {
                let generation = shared.generation.load(Ordering::Acquire);
                let stats = database.as_ref().map_or(Stats::default(), |d| d.stats);
                let event = SearchEvent { generation, paths: Vec::new(), scanned: stats.scanned, indexed: stats.indexed,
                    denied: stats.denied, skipped_reparse: stats.skipped_reparse, scanning: false, finished: true, truncated: false,
                    error: Some(format!("文件名索引未能启动：{error:#}")), reset: false,
                    backend: native.as_ref().map_or(BackendKind::DirectoryScan, metadata::Engine::backend),
                    phase: IndexPhase::Paused, volumes: native.as_ref().map_or_else(Vec::new, |m| m.statuses(false)),
                    read_only: ownership.as_ref().is_some_and(|o| !o.writer) };
                let _ = sender.try_send(event); shared.notify(); paused = true; needs_setup = false;
            }
            did_work = true;
        }
        if let Some(db) = &mut database {
            let tick = (|| -> Result<()> {
                if shared.stop.load(Ordering::Acquire) { return Ok(()); }
                if db.read_only && last_reader_poll.elapsed() >= Duration::from_millis(750)
                    && active.as_ref().is_none_or(|q| q.pass_complete() || q.truncated || q.error.is_some()) {
                    let version: i64 = db.connection.query_row("PRAGMA data_version", [], |r| r.get(0))?;
                    if version != reader_version {
                        let next = db.read_status()?;
                        let names_changed = next.revision != reader_progress.revision;
                        reader_progress = next; reader_version = version;
                        if names_changed { cache = None; db.revision += 1; }
                        if let Some(request) = last_request.as_ref().filter(|r| names_changed && shared.current(r.generation)) {
                            let mut query = QueryWork::new(request, db)?; query.reset = true;
                            active = Some(query);
                        }
                    }
                    last_reader_poll = Instant::now();
                }
                if let Some(query) = active.as_mut().filter(|q| shared.current(q.generation)) {
                    let query_budget = Instant::now();
                    for _ in 0..8 {
                        if query.error.is_some() || query.truncated ||
                            (query.cursor >= query.max_id && query.metadata_cursor >= query.metadata_max_id) || !shared.current(query.generation) { break; }
                        if query.metadata_cursor < query.metadata_max_id { query_metadata_page(db, query, &roots, &shared)?; }
                        else { query_page(db, query, &roots, &shared)?; }
                        did_work = true;
                        if query_budget.elapsed() >= Duration::from_millis(4) { break; }
                    }
                }
                if !paused && !db.read_only {
                    if let Some(native) = &mut native {
                        if db.batches % 32 == 0 && free_bytes(db.path.parent().unwrap_or(Path::new("."))).is_some_and(|free| free < MIN_FREE_BYTES) {
                            bail!("索引所在磁盘可用空间不足 512 MB");
                        }
                        db.check_wal()?;
                        shared.trace(13);
                        let update = native.tick(&mut db.connection, &shared.stop)?;
                        repair_pending |= update.repair_directories;
                        if update.worked { did_work = true; db.batches += 1; db.check_wal()?; }
                        if update.changed {
                            db.revision += 1; cache = None;
                            if let Some(request) = last_request.as_ref().filter(|r| shared.current(r.generation)) {
                                let mut restarted = QueryWork::new(request, db)?; restarted.reset = true; active = Some(restarted);
                            }
                        }
                    }
                    if shared.stop.load(Ordering::Acquire) { return Ok(()); }
                    if repair_pending && !db.stats.scanning {
                        current = None;
                        db.start_scan(&roots, last_request.as_ref().and_then(|r| r.preferred.as_deref()), true, false)?;
                        repair_pending = false; did_work = true;
                    }
                    if db.stats.scanning {
                        shared.trace(14);
                        scan_batch(db, &mut current, &mut active, &roots, &shared)?; did_work = true;
                    }
                    if !db.stats.scanning && !repair_pending { if let Some(native) = &mut native { native.coverage_complete(); } }
                    if !db.stats.scanning && db.result_repair {
                        cache = None; db.result_repair = false;
                        if let Some(request) = last_request.as_ref().filter(|r| shared.current(r.generation)) {
                            let mut query = QueryWork::new(request, db)?; query.reset = true;
                            if options.backend == BackendPreference::DirectoryScan { query.metadata_max_id = 0; }
                            active = Some(query);
                        }
                    }
                }
                Ok(())
            })();
            if let Err(error) = tick {
                // Restart a directory iterator after a rolled-back write: entries
                // already fetched from ReadDir must not disappear on a busy retry.
                current = None;
                let retry = sqlite_busy(&error) && busy_since.get_or_insert_with(Instant::now).elapsed() < Duration::from_secs(5);
                if retry { did_work = false; }
                else {
                    paused = true; db.stats.scanning = false;
                    maintenance_error=Some(format!("索引维护已暂停，结果仅覆盖已收录文件：{error:#}"));
                }
            } else {
                busy_since = None;
            }
            let progress = if db.read_only { reader_progress.clone() } else { Progress::writer(db.stats, native.as_ref(), paused, repair_pending, db.revision) };
            let status_transition = saved_progress.as_ref().is_none_or(|old| old.stats.scanning != progress.stats.scanning || old.view.phase != progress.view.phase);
            if !db.read_only && !paused && (status_transition || last_status_write.elapsed() >= Duration::from_millis(500)) && saved_progress.as_ref() != Some(&progress) {
                if let Err(error) = db.write_status(&progress) {
                    paused = true;
                    if let Some(query) = &mut active { query.error = Some(format!("索引状态写入失败：{error:#}")); }
                } else { saved_progress = Some(progress.clone()); }
                last_status_write = Instant::now();
            }
            if let Some(query) = active.as_mut().filter(|q| shared.current(q.generation)) {
                if ((query.cursor>=query.max_id&&query.metadata_cursor>=query.metadata_max_id)||query.truncated)&&query.error.is_none(){query.error=maintenance_error.clone();}
                // A repeated query can reuse the completed database pass while the
                // background filesystem pass continues to validate and add names.
                if ((query.cursor >= query.max_id && query.metadata_cursor >= query.metadata_max_id) || query.truncated) && query.error.is_none() {
                    if let Some(cached) = cache.as_mut().filter(|cached| cached.query == query.query) {
                        if cached.paths.len() != query.paths.len() || cached.truncated != query.truncated {
                            cached.paths = query.paths.clone(); cached.truncated = query.truncated;
                        }
                        cached.revision = db.revision;
                    } else {
                        cache = Some(CachedQuery { query: query.query.clone(), revision: db.revision, paths: query.paths.clone(), truncated: query.truncated });
                    }
                }
                shared.trace(15);
                publish(query, &progress, &sender, &shared);
            }
        }
        if waiting_since.is_some() && database.is_none() { did_work = false; }
        if !did_work {
            let guard = shared.control.lock().unwrap_or_else(|e| e.into_inner());
            if guard.request.is_none() && guard.action == ScanAction::None && !guard.release_idle && !shared.stop.load(Ordering::Acquire) {
                shared.trace(1600+u64::from(active.is_some())+u64::from(started)*2+u64::from(paused)*4);
                if active.as_ref().is_some_and(|query| !query.final_sent) {
                    drop(shared.wake.wait_timeout(guard, Duration::from_millis(40)));
                } else if waiting_since.is_some() || database.as_ref().is_some_and(|db| db.read_only)
                    || (started && !paused) {
                    drop(shared.wake.wait_timeout(guard, Duration::from_millis(250)));
                } else { drop(shared.wake.wait(guard)); }
            }
        }
    }
    drop(current);
    if let Some(mut database) = database {
        let _ = database.persist_progress();
        if !database.read_only { let _ = database.write_status(&Progress::writer(database.stats, native.as_ref(), paused, repair_pending, database.revision)); }
        database.checkpoint();
    }
    drop(native);
    drop(ownership);
}

fn sqlite_busy(error: &anyhow::Error) -> bool {
    matches!(error.downcast_ref::<rusqlite::Error>(), Some(rusqlite::Error::SqliteFailure(code, _))
        if matches!(code.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked))
}

fn query_metadata_page(db: &mut Database, query: &mut QueryWork, roots: &[PathBuf], shared: &Shared) -> Result<()> {
    let end = (query.metadata_cursor + QUERY_ID_WINDOW).min(query.metadata_max_id);
    let candidates: Vec<(String, Vec<u8>)> = {
        let mut statement = db.connection.prepare_cached("SELECT n.volume,n.frn FROM ntfs_nodes n
            JOIN ntfs_volumes v ON v.volume=n.volume WHERE n.id>?1 AND n.id<=?2
            AND v.available=1 AND v.phase>=3 AND length(n.frn)=8 AND length(n.volume)<=32
            AND length(n.folded)<=2048 AND instr(n.folded,?3)>0 ORDER BY n.id")?;
        let rows = statement.query_map(params![query.metadata_cursor,end,query.query], |r| Ok((r.get(0)?,r.get(1)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    for (volume, file_id) in candidates {
        if !shared.current(query.generation) { return Ok(()); }
        let id = u64::from_le_bytes(file_id.try_into().map_err(|_| anyhow::anyhow!("NTFS 文件标识无效"))?);
        if let Some(path) = metadata::resolve_path(&db.connection, &volume, id)? {
            match verified_result(&path, roots) {
                Ok(Some(path)) => query.add(path),
                Ok(None) => {},
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
                Err(_) => db.stats.denied += 1,
            }
        }
        if query.truncated { break; }
    }
    query.metadata_cursor = end;
    Ok(())
}

fn query_page(db: &mut Database, query: &mut QueryWork, roots: &[PathBuf], shared: &Shared) -> Result<()> {
    let end = (query.cursor + QUERY_ID_WINDOW).min(query.max_id);
    let mut stale = Vec::new();
    let mut next_cursor = end;
    let candidates = {
        let mut statement = db.connection.prepare_cached("SELECT e.id,d.path,e.name FROM entries e JOIN directories d ON d.id=e.directory_id
            WHERE e.id>?1 AND e.id<=?2 AND length(d.path)<=65534 AND length(e.name)<=510
            AND length(e.folded)<=2048 AND instr(e.folded,?3)>0 ORDER BY e.id")?;
        let mut rows = statement.query(params![query.cursor, end, query.query])?;
        let mut candidates = Vec::new();
        let mut bytes = 0usize;
        let mut last_id = query.cursor;
        while let Some(row) = rows.next()? {
            if !shared.current(query.generation) { return Ok(()); }
            let id: i64 = row.get(0)?;
            let directory: Vec<u8> = row.get(1)?;
            let name: Vec<u8> = row.get(2)?;
            let cost = directory.len().saturating_add(name.len()).saturating_add(96);
            if !candidates.is_empty() && bytes.saturating_add(cost) > RESULT_MEMORY_BYTES {
                next_cursor = last_id; break;
            }
            bytes += cost; last_id = id; candidates.push((id, directory, name));
        }
        candidates
    };
    // No SQLite read transaction spans a filesystem query. A slow disk must not
    // pin another process's WAL and exhaust the bounded write-log budget.
    for (id, directory, name) in candidates {
        if !shared.current(query.generation) { return Ok(()); }
        let path = PathBuf::from(decode_os(&directory)?).join(decode_os(&name)?);
        if containing_root(&path, roots).is_none() { continue; }
        match verified_result(&path, roots) {
            Ok(Some(path)) => query.add(path),
            Ok(None) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => stale.push(id),
            Err(_) => db.stats.denied += 1,
        }
        if query.truncated { break; }
    }
    if !db.read_only {
        for id in stale {
            let removed = db.connection.execute("DELETE FROM entries WHERE id=?1", [id])?;
            db.stats.indexed = db.stats.indexed.saturating_sub(removed as u64); db.revision += removed as u64;
        }
    }
    query.cursor = next_cursor;
    Ok(())
}

fn scan_batch(db: &mut Database, current: &mut Option<DirectoryWork>, active: &mut Option<QueryWork>, roots: &[PathBuf], shared: &Shared) -> Result<()> {
    if current.is_none() { *current = db.next_directory()?; }
    let Some(directory) = current.as_mut() else {
        db.stats.scanning = false; set_meta(&db.connection, "scan_active", 0)?; db.persist_progress()?; db.checkpoint(); return Ok(());
    };
    if directory.iterator.is_none() {
        let open = ordinary_directory_chain(&directory.path).and_then(|safe| {
            if !safe {
                return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "非普通目录或重解析点"));
            }
            fs::read_dir(&directory.path)
        });
        match open {
            Ok(iterator) => directory.iterator = Some(iterator),
            Err(_) => { db.stats.denied += 1; directory.failed = true; db.finish_directory(directory)?; *current = None; return Ok(()); }
        }
    }
    let mut batch = Vec::with_capacity(SCAN_BATCH);
    let mut exhausted = false;
    let started = Instant::now();
    for _ in 0..SCAN_BATCH {
        if shared.stop.load(Ordering::Acquire) { return Ok(()); }
        let Some(next) = directory.iterator.as_mut().unwrap().next() else { exhausted = true; break };
        db.stats.scanned += 1;
        match next {
            Ok(entry) => match entry.metadata() {
                Ok(metadata) => {
                    let reparse = is_reparse(metadata.file_attributes());
                    if reparse { db.stats.skipped_reparse += 1; }
                    let name = entry.file_name();
                    batch.push(ScannedEntry { folded: name.to_string_lossy().to_lowercase(), name,
                        directory: metadata.is_dir(), reparse });
                }
                Err(_) => { db.stats.denied += 1; directory.failed = true; }
            },
            Err(_) => { db.stats.denied += 1; directory.failed = true; }
        }
        if started.elapsed() >= Duration::from_millis(12) { break; }
    }
    if !batch.is_empty() {
        if db.batches % 32 == 0 && free_bytes(db.path.parent().unwrap_or(Path::new("."))).is_some_and(|free| free < MIN_FREE_BYTES) {
            bail!("索引所在磁盘可用空间不足 512 MB");
        }
        db.check_wal()?;
        let transaction = db.connection.transaction()?;
        let mut inserted = 0u64;
        for entry in &batch {
            let name = encode_os(&entry.name);
            let added = transaction.execute("INSERT OR IGNORE INTO entries(directory_id,name,folded,is_dir,reparse,seen) VALUES(?1,?2,?3,?4,?5,?6)",
                params![directory.id, name, entry.folded, entry.directory, entry.reparse, db.epoch])?;
            if added == 0 {
                transaction.execute("UPDATE entries SET seen=?3,is_dir=?4,reparse=?5 WHERE directory_id=?1 AND name=?2 AND seen<>?3",
                    params![directory.id, name, db.epoch, entry.directory, entry.reparse])?;
            } else { inserted += 1; }
            if entry.directory && !entry.reparse {
                queue_directory(&transaction, &directory.path.join(&entry.name), directory.root, directory.priority, db.epoch)?;
            }
        }
        transaction.commit()?;
        db.stats.indexed += inserted; db.revision += inserted;
        db.batches += 1;
        db.check_wal()?;
        if let Some(query) = active.as_mut().filter(|q| shared.current(q.generation) && !q.truncated && q.error.is_none()) {
            for entry in &batch {
                if entry.folded.contains(&query.query) {
                    let path = directory.path.join(&entry.name);
                    if let Ok(Some(path)) = verified_result(&path, roots) { query.add(path); }
                    if query.truncated { break; }
                }
            }
        }
    }
    if exhausted { db.finish_directory(directory)?; *current = None; }
    Ok(())
}

fn publish(query: &mut QueryWork, progress: &Progress, sender: &SyncSender<SearchEvent>, shared: &Shared) -> bool {
    let stats = progress.stats;
    if !shared.current(query.generation) { return false; }
    if !query.reset && query.final_sent && ((stats == query.last_stats && progress.view == query.last_view) ||
        (stats.scanning && query.last_progress.elapsed() < Duration::from_millis(500))) { return false; }
    let end = (query.delivered + EVENT_BATCH).min(query.paths.len());
    let data = end > query.delivered;
    let finished = query.done(stats) && end == query.paths.len();
    if !query.reset && !data && !finished && query.last_progress.elapsed() < Duration::from_millis(500) { return false; }
    let paths = query.paths[query.delivered..end].iter().filter(|p| fs::symlink_metadata(p).is_ok()).cloned().collect();
    let event = query.event(progress, paths, finished);
    match sender.try_send(event) {
        Ok(()) => { query.delivered = end; query.final_sent = finished; query.last_stats = stats;
            query.last_view = progress.view.clone(); query.reset = false; query.last_progress = Instant::now(); shared.notify(); true }
        Err(TrySendError::Full(_)) => false,
        Err(TrySendError::Disconnected(_)) => { shared.stop.store(true, Ordering::Release); false }
    }
}

fn fixed_roots() -> Vec<PathBuf> {
    let mut drives = [0u16; 512];
    let length = unsafe { GetLogicalDriveStringsW(Some(&mut drives)) } as usize;
    if length == 0 || length >= drives.len() { return Vec::new(); }
    drives[..length].split(|c| *c == 0).filter(|s| !s.is_empty()).filter_map(|drive| {
        let terminated: Vec<u16> = drive.iter().copied().chain(Some(0)).collect();
        (unsafe { GetDriveTypeW(PCWSTR(terminated.as_ptr())) } == 3).then(|| PathBuf::from(OsString::from_wide(drive)))
    }).collect()
}
fn containing_root(path: &Path, roots: &[PathBuf]) -> Option<usize> {
    if !path.is_absolute() || path.components().any(|part| matches!(part, Component::ParentDir | Component::CurDir)) { return None; }
    if !matches!(path.components().next(), Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_))) { return None; }
    let key = path_key(path).to_lowercase();
    roots.iter().position(|root| { let root = path_key(root).to_lowercase(); key == root || key.starts_with(&(root.trim_end_matches('\\').to_string() + "\\")) })
}
fn path_key(path: &Path) -> String {
    let path = plain_path(path);
    let mut text = String::new();
    for character in char::decode_utf16(path.as_os_str().encode_wide()) {
        match character {
            Ok('/') => text.push('\\'),
            Ok(character) => text.push(character),
            Err(error) => { text.push('\0'); text.push_str(&format!("{:04x}", error.unpaired_surrogate())); }
        }
    }
    if text.as_bytes().get(1) == Some(&b':') { text.replace_range(..1, &text[..1].to_ascii_uppercase()); }
    while text.len() > 3 && text.ends_with('\\') { text.pop(); }
    text
}
fn plain_path(path: &Path) -> PathBuf {
    if matches!(path.components().next(), Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::VerbatimDisk(_))) {
        PathBuf::from(OsString::from_wide(&path.as_os_str().encode_wide().skip(4).collect::<Vec<_>>()))
    } else { path.to_path_buf() }
}
fn ordinary_directory_chain(path: &Path) -> std::io::Result<bool> {
    let mut current = PathBuf::new();
    for part in path.components() {
        if matches!(part, Component::ParentDir | Component::CurDir) { return Ok(false); }
        current.push(part.as_os_str());
        if matches!(part, Component::Prefix(_)) { continue; }
        let metadata = fs::symlink_metadata(&current)?;
        if !metadata.is_dir() || is_reparse(metadata.file_attributes()) { return Ok(false); }
    }
    Ok(true)
}
fn existing_in_scope(path: &Path, roots: &[PathBuf]) -> std::io::Result<bool> {
    if containing_root(path, roots).is_none() { return Ok(false); }
    let Some(parent) = path.parent() else { return Ok(false) };
    if !ordinary_directory_chain(parent)? { return Ok(false); }
    fs::symlink_metadata(path).map(|_| true)
}
fn verified_result(path: &Path, roots: &[PathBuf]) -> std::io::Result<Option<PathBuf>> {
    if !existing_in_scope(path, roots)? { return Ok(None); }
    let attributes = fs::symlink_metadata(path)?.file_attributes();
    // Preserve a reparse entry itself; never replace its displayed name with a
    // target outside the indexed tree. Ordinary names use their current spelling,
    // deduplicating cached paths after a case-only directory/file rename.
    if is_reparse(attributes) { return Ok(Some(path.to_path_buf())); }
    match fs::canonicalize(path) {
        Ok(canonical) => {
            let canonical = plain_path(&canonical);
            if containing_root(&canonical, roots).is_none() { return Ok(None); }
            Ok(Some(canonical))
        }
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => Ok(Some(path.to_path_buf())),
        Err(error) => Err(error),
    }
}
fn encode_os(value: &OsStr) -> Vec<u8> { value.encode_wide().flat_map(u16::to_le_bytes).collect() }
fn decode_os(value: &[u8]) -> Result<OsString> {
    if value.len() % 2 != 0 { bail!("索引路径编码无效，请重建文件名索引"); }
    Ok(OsString::from_wide(&value.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect::<Vec<_>>()))
}
fn is_reparse(attributes: u32) -> bool { attributes & REPARSE_POINT != 0 }
fn free_bytes(path: &Path) -> Option<u64> {
    let text: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut available = 0u64;
    unsafe { GetDiskFreeSpaceExW(PCWSTR(text.as_ptr()), Some(&mut available), None, None) }.ok().map(|_| available)
}

#[cfg(test)]
mod tests {
    use super::*;
    static NEXT: AtomicU64 = AtomicU64::new(1);
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            Self::under(&std::env::temp_dir())
        }
        fn under(parent: &Path) -> Self {
            let root = parent.join(format!("zsdesk-file-index-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
            fs::create_dir(&root).unwrap();
            // Windows TEMP can contain an 8.3 alias. Results use canonical
            // spelling, so fixture roots and expected paths must use it too.
            Self(plain_path(&fs::canonicalize(&root).unwrap()))
        }
        fn service(&self) -> FastSearch {
            FastSearch::with_roots(self.0.join("index/search-index.sqlite"), HWND::default(), 0, Some(vec![self.0.join("files")])).unwrap()
        }
    }
    impl Drop for Fixture { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); } }
    fn collect(service: &FastSearch, generation: u64) -> (HashSet<PathBuf>, SearchEvent) {
        let start = Instant::now(); let mut paths = HashSet::new();let mut last_event=None;
        while start.elapsed() < Duration::from_secs(20) {
            for event in service.poll() {
                assert_eq!(event.generation, generation); assert!(event.paths.len() <= EVENT_BATCH);
                assert!(event.error.is_none(), "{:?}", event.error);
                if event.reset { paths.clear(); }
                paths.extend(event.paths.iter().cloned());
                if event.finished { return (paths, event); }
                last_event=Some(event);
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("file-name index fixture timed out: role={:?} stage={} current={} requested={generation} last={last_event:?}",service.maintenance_role(),service.shared.trace_stage.load(Ordering::Relaxed),service.shared.generation.load(Ordering::Acquire));
    }
    fn stop(mut service: FastSearch) {
        let control = service.shared.control.lock().unwrap_or_else(|e| e.into_inner());
        service.shared.stop.store(true, Ordering::Release);
        drop(control); service.shared.wake.notify_all();
        if let Some(worker) = service.worker.take() { worker.join().unwrap(); }
    }
    #[test]
    fn temporary_root_aliases_keep_explicit_fixture_queries_in_scope() {
        #[link(name="kernel32")]
        unsafe extern "system" {
            fn GetShortPathNameW(long: *const u16, short: *mut u16, capacity: u32) -> u32;
        }
        let parent=Fixture::new();
        let name:Vec<u16>=parent.0.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut short=vec![0u16;32768];
        let length=unsafe{GetShortPathNameW(name.as_ptr(),short.as_mut_ptr(),short.len()as u32)}as usize;
        // Volumes without short-name support still exercise the same explicit
        // scope and canonical-root contract; no environment variables change.
        let alias=if length>0&&length<short.len(){PathBuf::from(OsString::from_wide(&short[..length]))}else{parent.0.clone()};
        let fixture=Fixture::under(&alias);
        assert_eq!(fixture.0,plain_path(&fs::canonicalize(&fixture.0).unwrap()));
        assert!(matches!(fixture.0.components().next(),Some(Component::Prefix(prefix))if matches!(prefix.kind(),Prefix::Disk(_))));
        let files=fixture.0.join("files");fs::create_dir(&files).unwrap();
        let expected=files.join("显式范围 中文.txt");fs::write(&expected,b"fixture").unwrap();
        let service=fixture.service();
        let generation=service.start("显式范围",Some(files));
        let(paths,event)=collect(&service,generation);
        assert_eq!(paths,HashSet::from([expected]));assert!(!event.scanning);
        stop(service);
    }
    #[test]
    fn persistent_index_finds_unicode_names_and_removes_dead_results() {
        let fixture = Fixture::new(); let files = fixture.0.join("files"); fs::create_dir_all(files.join("子目录")).unwrap();
        let first = files.join("资料 核验甲.txt"); let second = files.join("子目录/资料 核验乙.txt");
        fs::write(&first, b"a").unwrap(); fs::write(&second, b"b").unwrap(); fs::write(files.join("unmatched.txt"), "资料 核验").unwrap();
        let service = fixture.service(); let generation = service.start("资料 核验", Some(files.clone()));
        let (paths, event) = collect(&service, generation); assert_eq!(paths, HashSet::from([first.clone(), second.clone()]));
        assert!(!event.scanning && !event.truncated);
        let repeated = service.start("资料 核验", Some(files.clone()));
        let (again, repeated_event) = collect(&service, repeated);
        assert_eq!(again, paths); assert_eq!(repeated_event.scanned, event.scanned, "an ordinary repeated query must not rescan the filesystem");
        stop(service);
        let connection = Connection::open(fixture.0.join("index/search-index.sqlite")).unwrap();
        assert!(connection.query_row("SELECT count(*) FROM entries", [], |r| r.get::<_, i64>(0)).unwrap() >= 4); drop(connection);
        fs::remove_file(&first).unwrap();
        let service = fixture.service(); let generation = service.start("资料 核验", None);
        let (paths, _) = collect(&service, generation); assert_eq!(paths, HashSet::from([second]));
        stop(service);
    }
    #[test]
    fn newest_generation_wins_and_cancel_does_not_wait_for_scan() {
        let fixture = Fixture::new(); let files = fixture.0.join("files"); fs::create_dir(&files).unwrap();
        for i in 0..80 { fs::write(files.join(format!("alpha-{i}.txt")), b"").unwrap(); }
        let beta = files.join("beta 中文.txt"); fs::write(&beta, b"").unwrap();
        let service = fixture.service(); let old = service.start("alpha", Some(files.clone())); let generation = service.start("beta", Some(files));
        assert!(generation > old); let (paths, _) = collect(&service, generation); assert_eq!(paths, HashSet::from([beta]));
        let now = Instant::now(); service.cancel(); assert!(now.elapsed() < Duration::from_millis(100)); assert!(service.poll().is_empty());
        stop(service);
    }
    #[test]
    fn preferred_root_does_not_drop_other_roots_and_refresh_adds_new_names() {
        let fixture = Fixture::new();
        let a = fixture.0.join("范围 甲"); let b = fixture.0.join("开发 target 范围 乙");
        fs::create_dir(&a).unwrap(); fs::create_dir(&b).unwrap();
        let first = a.join("文件 核验甲.txt"); let second = b.join("文件 核验乙.txt");
        fs::write(&first, b"").unwrap(); fs::write(&second, b"").unwrap();
        let service = FastSearch::new_with_roots_for_test(fixture.0.join("index/search-index.sqlite"), vec![a.clone(), b.clone()]).unwrap();
        let generation = service.start("文件 核验", Some(b.clone()));
        let (paths, _) = collect(&service, generation); assert_eq!(paths, HashSet::from([first.clone(), second.clone()]));
        let third = a.join("文件 核验新建.txt"); fs::write(&third, b"").unwrap();
        service.refresh(); let generation = service.start("文件 核验", Some(a));
        let (paths, _) = collect(&service, generation); assert_eq!(paths, HashSet::from([first, second, third]));
        stop(service);
    }
    #[test]
    fn result_batches_and_database_cache_have_explicit_bounds() {
        let fixture = Fixture::new(); let files = fixture.0.join("files"); fs::create_dir(&files).unwrap();
        for i in 0..2055 { fs::write(files.join(format!("match-{i:04}.txt")), b"").unwrap(); }
        let service = fixture.service(); let generation = service.start("match-", Some(files)); let (paths, event) = collect(&service, generation);
        assert_eq!(paths.len(), MAX_RESULTS); assert!(event.truncated); stop(service);
        let db = Database::open(&fixture.0.join("index/search-index.sqlite")).unwrap();
        assert_eq!(db.connection.query_row("PRAGMA cache_size", [], |r| r.get::<_, i64>(0)).unwrap(), -PAGE_CACHE_KIB);
        assert_eq!(db.connection.query_row("PRAGMA mmap_size", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
        assert_eq!(db.connection.query_row("PRAGMA max_page_count", [], |r| r.get::<_, i64>(0)).unwrap(), MAX_DATABASE_PAGES);
    }
    #[test]
    fn durable_alias_repair_requeues_completed_directories_after_restart() {
        let fixture = Fixture::new(); let path = fixture.0.join("index/search-index.sqlite");
        let roots = vec![fixture.0.join("files")]; let mut db = Database::open(&path).unwrap();
        db.start_scan(&roots,None,true,false).unwrap(); let epoch = db.epoch;
        db.connection.execute("UPDATE directories SET completed=?1,pending=0",[epoch]).unwrap();
        set_meta(&db.connection,"scan_active",1).unwrap(); set_meta(&db.connection,"directory_repair",1).unwrap(); drop(db);
        let mut db = Database::open(&path).unwrap(); db.start_scan(&roots,None,false,false).unwrap();
        assert!(db.epoch>epoch); assert_eq!(meta(&db.connection,"directory_repair").unwrap(),0);
        assert_eq!(db.connection.query_row("SELECT pending FROM directories",[],|r|r.get::<_,i64>(0)).unwrap(),db.epoch);
    }
    #[test]
    fn auto_fallback_preserves_hardlink_aliases_and_notifies_without_hwnd() {
        let fixture = Fixture::new(); let files = fixture.0.join("files");
        fs::create_dir_all(files.join("other")).unwrap();
        let first = files.join("alias-first.txt"); let second = files.join("other/alias-second.txt");
        fs::write(&first,b"fixture").unwrap(); fs::hard_link(&first,&second).unwrap();
        let notices = Arc::new(AtomicU64::new(0)); let callback = notices.clone();
        let service = FastSearch::with_options_and_roots(fixture.0.join("index/search-index.sqlite"),HWND::default(),0,
            Some(vec![files.clone()]),SearchOptions::default(),Some(Arc::new(move || { callback.fetch_add(1,Ordering::Relaxed); }))).unwrap();
        let generation = service.start("alias-",None); let (paths,event) = collect(&service,generation);
        assert_eq!(paths,HashSet::from([first,second])); assert_eq!(event.backend,BackendKind::DirectoryScan);
        assert!(event.volumes.iter().all(|v|v.warning.is_some())); assert!(notices.load(Ordering::Relaxed)>0);
        stop(service);
        let database = Database::open(&fixture.0.join("index/search-index.sqlite")).unwrap();
        let before: i64 = database.connection.query_row("SELECT count(*) FROM entries",[],|r|r.get(0)).unwrap();
        metadata::clear(&database.connection).unwrap();
        assert_eq!(database.connection.query_row("SELECT count(*) FROM entries",[],|r|r.get::<_,i64>(0)).unwrap(),before);
    }
    #[test]
    fn shared_index_readers_cannot_rebuild_or_cancel_the_writer_and_can_take_over() {
        let fixture = Fixture::new(); let files = fixture.0.join("files"); fs::create_dir(&files).unwrap();
        let alpha = files.join("alpha.txt"); let beta = files.join("beta.txt"); fs::write(&alpha,b"").unwrap(); fs::write(&beta,b"").unwrap();
        let writer = fixture.service(); let generation = writer.start("alpha",None); collect(&writer,generation);
        assert_eq!(writer.maintenance_role(),MaintenanceRole::Writer);
        let reader = fixture.service(); let read_generation = reader.start("alpha",None);
        let (paths,event) = collect(&reader,read_generation); assert_eq!(paths,HashSet::from([alpha])); assert!(event.read_only);
        assert_eq!(writer.maintenance_role(),MaintenanceRole::Writer);assert_eq!(reader.maintenance_role(),MaintenanceRole::Reader);
        reader.cancel();
        let release_deadline=Instant::now()+Duration::from_secs(3);
        while reader.maintenance_role()!=MaintenanceRole::Pending{assert!(Instant::now()<release_deadline,"cancelled reader retained ownership");thread::sleep(Duration::from_millis(5));}
        assert_eq!(writer.maintenance_role(),MaintenanceRole::Writer);
        let resumed=reader.start("alpha",None);collect(&reader,resumed);
        // A new query supersedes the maintenance generation. Observe the
        // rejection before starting it, otherwise a fast rejection is correctly
        // discarded as stale and the test races the worker's scheduling.
        let rejected = reader.rebuild(); let deadline = Instant::now()+Duration::from_secs(5); let mut refused = false;
        while Instant::now()<deadline {
            for event in reader.poll() { if event.generation==rejected && event.error.is_some() { refused = true; assert!(event.read_only); } }
            if refused { break; } thread::sleep(Duration::from_millis(5));
        }
        assert!(refused);
        let generation=reader.start("beta",None);let (paths,event)=collect(&reader,generation);
        assert_eq!(paths,HashSet::from([beta.clone()]));assert!(event.read_only);
        let db = Database::open_read_only(&fixture.0.join("index/search-index.sqlite")).unwrap();
        assert_eq!(db.connection.query_row("SELECT count(*) FROM entries",[],|r|r.get::<_,i64>(0)).unwrap(),2); drop(db);
        stop(writer);
        let generation = reader.start("beta",None); let (paths,event) = collect(&reader,generation);
        assert_eq!(paths,HashSet::from([beta])); assert!(!event.read_only);assert_eq!(reader.maintenance_role(),MaintenanceRole::Writer);stop(reader);
    }
    #[test]
    fn maintenance_role_is_reported_without_an_active_query() {
        let fixture=Fixture::new();fs::create_dir(fixture.0.join("files")).unwrap();
        let engine=fixture.service();assert_eq!(engine.maintenance_role(),MaintenanceRole::Pending);engine.refresh();
        let deadline=Instant::now()+Duration::from_secs(5);
        while engine.maintenance_role()==MaintenanceRole::Pending { assert!(Instant::now()<deadline);thread::sleep(Duration::from_millis(5)); }
        assert_eq!(engine.maintenance_role(),MaintenanceRole::Writer);engine.cancel();
        let release_deadline=Instant::now()+Duration::from_secs(3);
        while engine.maintenance_role()!=MaintenanceRole::Pending{assert!(Instant::now()<release_deadline,"idle index writer was not released");thread::sleep(Duration::from_millis(5));}
        let shared=engine.shared.clone();stop(engine);assert_eq!(shared.maintenance_role.load(Ordering::Acquire),0);
    }
    #[test]
    fn path_boundary_and_reparse_rules_do_not_exclude_named_directories() {
        let roots = vec![PathBuf::from(r"E:\资料")];
        assert!(containing_root(Path::new(r"e:\资料\开发\target\中文.txt"), &roots).is_some());
        assert!(containing_root(Path::new(r"E:\资料旧\file.txt"), &roots).is_none());
        assert!(containing_root(Path::new(r"E:\资料\..\别处\file.txt"), &roots).is_none());
        assert!(containing_root(Path::new(r"\\server\资料\file.txt"), &roots).is_none());
        assert!(is_reparse(0x410)); assert!(!is_reparse(0x10));
        let name = OsString::from_wide(&[0x4E2D,0x6587,0x20,0xD800]);
        assert_eq!(decode_os(&encode_os(&name)).unwrap(), name);
    }
}
