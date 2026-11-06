//! Persistent file-name search. Files are enumerated on one worker; contents are never read.

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
}

#[derive(Clone)]
struct Request { generation: u64, query: String, preferred: Option<PathBuf>, invalid: Option<String> }
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum ScanAction { #[default] None, Refresh, Rebuild }
#[derive(Default)]
struct Control { request: Option<Request>, action: ScanAction }
struct Shared {
    control: Mutex<Control>, wake: Condvar, stop: AtomicBool, generation: AtomicU64,
    notified: AtomicBool, hwnd: isize, message: u32,
}

pub struct FastSearch { shared: Arc<Shared>, events: Receiver<SearchEvent>, worker: Option<JoinHandle<()>> }

impl FastSearch {
    /// Construction does not scan disks or open the index; the first `start` does so on the worker.
    pub fn new(index_path: PathBuf, hwnd: HWND, message_id: u32) -> Result<Self> {
        Self::with_roots(index_path, hwnd, message_id, None)
    }

    #[cfg(test)]
    pub(crate) fn new_with_roots_for_test(index_path: PathBuf, roots: Vec<PathBuf>) -> Result<Self> {
        Self::with_roots(index_path, HWND::default(), 0, Some(roots))
    }

    fn with_roots(index_path: PathBuf, hwnd: HWND, message_id: u32, roots: Option<Vec<PathBuf>>) -> Result<Self> {
        let shared = Arc::new(Shared { control: Mutex::new(Control::default()), wake: Condvar::new(),
            stop: AtomicBool::new(false), generation: AtomicU64::new(0), notified: AtomicBool::new(false),
            hwnd: hwnd.0 as isize, message: message_id });
        let (sender, events) = mpsc::sync_channel(EVENT_QUEUE);
        let state = shared.clone();
        let worker = thread::Builder::new().name("zsdesk-file-index".into())
            .spawn(move || worker_loop(index_path, roots, state, sender)).context("无法启动文件名索引线程")?;
        Ok(Self { shared, events, worker: Some(worker) })
    }

    /// Replaces an earlier request immediately. Empty input cancels the query.
    pub fn start(&self, query: &str, preferred: Option<PathBuf>) -> u64 {
        let generation = self.shared.generation.fetch_add(1, Ordering::AcqRel) + 1;
        self.discard_events();
        let text: String = query.trim().chars().take(513).collect();
        let invalid = if text.chars().count() > 512 { Some("搜索内容过长，请缩短文件名关键词。".into()) }
            else if text.contains('\0') { Some("搜索内容包含无效字符。".into()) } else { None };
        let request = (!text.is_empty()).then(|| Request { generation, query: text.to_lowercase(), preferred, invalid });
        self.shared.control.lock().unwrap_or_else(|e| e.into_inner()).request = request;
        self.shared.wake.notify_one();
        generation
    }

    /// Cancels the query, not an already requested background index pass.
    pub fn cancel(&self) {
        self.shared.generation.fetch_add(1, Ordering::AcqRel);
        self.shared.control.lock().unwrap_or_else(|e| e.into_inner()).request = None;
        self.discard_events();
        self.shared.wake.notify_one();
    }

    /// Retains searchable records and schedules a new file-system pass. Call `start` for the new view.
    pub fn refresh(&self) { self.schedule_scan(ScanAction::Refresh); }

    /// Clears only the owned index records and rebuilds them. Never removes source files.
    pub fn rebuild(&self) { self.schedule_scan(ScanAction::Rebuild); }

    fn schedule_scan(&self, action: ScanAction) {
        self.shared.generation.fetch_add(1, Ordering::AcqRel);
        let mut control = self.shared.control.lock().unwrap_or_else(|e| e.into_inner());
        control.request = None;
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
    fn notify(&self) {
        if self.stop.load(Ordering::Acquire) || self.hwnd == 0 || self.message == 0 || self.notified.swap(true, Ordering::AcqRel) { return; }
        if unsafe { PostMessageW(Some(HWND(self.hwnd as *mut _)), self.message, WPARAM(0), LPARAM(0)) }.is_err() {
            self.notified.store(false, Ordering::Release);
        }
    }
    fn current(&self, generation: u64) -> bool {
        !self.stop.load(Ordering::Acquire) && self.generation.load(Ordering::Acquire) == generation
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct Stats { scanned: u64, indexed: u64, denied: u64, skipped_reparse: u64, scanning: bool }
struct Database { connection: Connection, path: PathBuf, stats: Stats, revision: u64, epoch: i64, batches: u64 }

impl Database {
    fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() { fs::create_dir_all(parent).context("无法创建索引目录")?; }
        let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE | OpenFlags::SQLITE_OPEN_NO_MUTEX)
            .context("无法打开文件名索引")?;
        connection.busy_timeout(Duration::from_millis(250))?;
        let application: i64 = connection.query_row("PRAGMA application_id", [], |r| r.get(0))?;
        let tables: i64 = connection.query_row("SELECT count(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'", [], |r| r.get(0))?;
        if application != APPLICATION_ID && (application != 0 || tables > 0) { bail!("索引文件不是可识别的 ZSDesk 文件名索引"); }
        connection.execute_batch(&format!("PRAGMA page_size=4096; PRAGMA cache_size=-{PAGE_CACHE_KIB}; PRAGMA mmap_size=0;
            PRAGMA temp_store=FILE; PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;
            PRAGMA wal_autocheckpoint=256; PRAGMA journal_size_limit=4194304; PRAGMA max_page_count={MAX_DATABASE_PAGES};
            PRAGMA application_id={APPLICATION_ID};"))?;
        let version: i64 = connection.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version > 1 { bail!("文件名索引由较新版本创建"); }
        connection.execute_batch("CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY,value INTEGER NOT NULL) WITHOUT ROWID;
            CREATE TABLE IF NOT EXISTS directories(id INTEGER PRIMARY KEY,path BLOB NOT NULL,path_key TEXT NOT NULL UNIQUE,
                root INTEGER NOT NULL,priority INTEGER NOT NULL,pending INTEGER NOT NULL DEFAULT 0,completed INTEGER NOT NULL DEFAULT 0);
            CREATE INDEX IF NOT EXISTS directories_pending ON directories(pending,priority,id);
            CREATE TABLE IF NOT EXISTS entries(id INTEGER PRIMARY KEY,directory_id INTEGER NOT NULL,name BLOB NOT NULL,
                folded TEXT NOT NULL,is_dir INTEGER NOT NULL,reparse INTEGER NOT NULL,seen INTEGER NOT NULL,
                UNIQUE(directory_id,name)); PRAGMA user_version=1;")?;
        let indexed = connection.query_row("SELECT count(*) FROM entries", [], |r| r.get::<_, i64>(0))? as u64;
        let epoch = meta(&connection, "epoch")?;
        Ok(Self { connection, path: path.to_path_buf(), stats: Stats { indexed, ..Default::default() }, revision: 0, epoch, batches: 0 })
    }

    fn start_scan(&mut self, roots: &[PathBuf], preferred: Option<&Path>, force: bool, clear: bool) -> Result<()> {
        if clear {
            self.connection.execute_batch("DELETE FROM entries; DELETE FROM directories;")?;
            self.stats.indexed = 0; self.revision += 1;
        }
        let resume = !force && !clear && meta(&self.connection, "scan_active")? == 1 && self.epoch > 0;
        if !resume {
            self.epoch = self.epoch.saturating_add(1).max(1);
            self.stats.scanned = 0; self.stats.denied = 0; self.stats.skipped_reparse = 0;
            set_meta(&self.connection, "epoch", self.epoch)?;
            for (root, path) in roots.iter().enumerate() { self.queue_directory(path, root, 10 + root as i64)?; }
        } else {
            self.stats.scanned = meta(&self.connection, "scanned")?.max(0) as u64;
            self.stats.denied = meta(&self.connection, "denied")?.max(0) as u64;
            self.stats.skipped_reparse = meta(&self.connection, "reparse")?.max(0) as u64;
        }
        self.stats.scanning = true;
        set_meta(&self.connection, "scan_active", 1)?;
        self.prioritize(roots, preferred)?;
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
        }
        self.connection.execute("UPDATE directories SET pending=0,completed=?2 WHERE id=?1", params![directory.id, self.epoch])?;
        self.persist_progress()?;
        Ok(())
    }

    fn persist_progress(&self) -> Result<()> {
        set_meta(&self.connection, "scanned", self.stats.scanned as i64)?;
        set_meta(&self.connection, "denied", self.stats.denied as i64)?;
        set_meta(&self.connection, "reparse", self.stats.skipped_reparse as i64)?;
        Ok(())
    }

    fn checkpoint(&self) { let _ = self.connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);"); }

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
    bytes: usize, delivered: usize, truncated: bool, final_sent: bool, error: Option<String>, last_progress: Instant,
    last_stats: Stats,
}

impl QueryWork {
    fn new(request: &Request, database: &Database) -> Result<Self> {
        let max_id = database.connection.query_row("SELECT coalesce(max(id),0) FROM entries", [], |r| r.get(0))?;
        Ok(Self { generation: request.generation, query: request.query.clone(), cursor: 0, max_id, paths: Vec::new(),
            seen: HashSet::new(), bytes: 0, delivered: 0, truncated: false, final_sent: false,
            error: request.invalid.clone(), last_progress: Instant::now() - Duration::from_secs(1), last_stats: Stats::default() })
    }
    fn add(&mut self, path: PathBuf) {
        let key = path_key(&path);
        if self.seen.contains(&key) { return; }
        // Count retained allocations, with room for the vector/hash-table slots.
        let bytes = path.capacity().saturating_add(key.capacity()).saturating_add(96);
        if self.paths.len() >= MAX_RESULTS || self.bytes.saturating_add(bytes) > RESULT_MEMORY_BYTES { self.truncated = true; return; }
        self.bytes += bytes; self.seen.insert(key); self.paths.push(path);
    }
    fn done(&self, stats: Stats) -> bool { self.error.is_some() || self.truncated || (self.cursor >= self.max_id && !stats.scanning) }
    fn event(&self, stats: Stats, paths: Vec<PathBuf>, finished: bool) -> SearchEvent {
        SearchEvent { generation: self.generation, paths, scanned: stats.scanned, indexed: stats.indexed,
            denied: stats.denied, skipped_reparse: stats.skipped_reparse, scanning: stats.scanning,
            finished, truncated: self.truncated, error: self.error.clone() }
    }
}

struct CachedQuery { query: String, revision: u64, paths: Vec<PathBuf>, truncated: bool }

fn worker_loop(path: PathBuf, roots_override: Option<Vec<PathBuf>>, shared: Arc<Shared>, sender: SyncSender<SearchEvent>) {
    let _ = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN) };
    let mut database: Option<Database> = None;
    let mut roots = Vec::new();
    let mut current: Option<DirectoryWork> = None;
    let mut active: Option<QueryWork> = None;
    let mut cache: Option<CachedQuery> = None;
    let mut started = false;
    let mut paused = false;
    let mut last_preferred: Option<String> = None;
    while !shared.stop.load(Ordering::Acquire) {
        let control = {
            let mut control = shared.control.lock().unwrap_or_else(|e| e.into_inner());
            std::mem::take(&mut *control)
        };
        if active.as_ref().is_some_and(|query| !shared.current(query.generation)) { active = None; }
        let mut did_work = false;
        if let Some(request) = control.request.as_ref().filter(|r| r.invalid.is_some()) {
            let event = SearchEvent { generation: request.generation, paths: Vec::new(), scanned: 0, indexed: 0, denied: 0,
                skipped_reparse: 0, scanning: database.as_ref().is_some_and(|db| db.stats.scanning), finished: true,
                truncated: false, error: request.invalid.clone() };
            let _ = sender.try_send(event); shared.notify();
            continue;
        }
        if control.request.is_some() || control.action != ScanAction::None {
            let setup = (|| -> Result<()> {
                if database.is_none() {
                    roots = roots_override.clone().unwrap_or_else(fixed_roots);
                    if roots.is_empty() { bail!("没有可建立索引的本地固定磁盘"); }
                    database = Some(Database::open(&path)?);
                }
                let db = database.as_mut().unwrap();
                if control.action != ScanAction::None {
                    current = None; active = None; cache = None;
                    db.start_scan(&roots, control.request.as_ref().and_then(|r| r.preferred.as_deref()), true, control.action == ScanAction::Rebuild)?;
                    started = true; paused = false;
                }
                if let Some(request) = &control.request {
                    if !shared.current(request.generation) { return Ok(()); }
                    if !started || paused {
                        db.start_scan(&roots, request.preferred.as_deref(), false, false)?;
                        started = true; paused = false;
                    } else if db.stats.scanning && request.preferred.as_ref().map(|p| path_key(p)) != last_preferred {
                        db.prioritize(&roots, request.preferred.as_deref())?;
                        // A preferred directory can preempt a large in-progress directory at a batch boundary.
                        if request.preferred.is_some() { current = None; }
                    }
                    last_preferred = request.preferred.as_ref().map(|p| path_key(p));
                    let mut query = QueryWork::new(request, db)?;
                    if let Some(cached) = cache.as_ref().filter(|c| c.query == request.query && c.revision == db.revision) {
                        for item in &cached.paths {
                            if existing_in_scope(item, &roots).unwrap_or(false) { query.add(item.clone()); }
                        }
                        query.truncated = cached.truncated;
                        query.cursor = query.max_id;
                    }
                    active = Some(query);
                }
                Ok(())
            })();
            if let Err(error) = setup {
                let generation = shared.generation.load(Ordering::Acquire);
                let stats = database.as_ref().map_or(Stats::default(), |d| d.stats);
                let event = SearchEvent { generation, paths: Vec::new(), scanned: stats.scanned, indexed: stats.indexed,
                    denied: stats.denied, skipped_reparse: stats.skipped_reparse, scanning: false, finished: true, truncated: false,
                    error: Some(format!("文件名索引未能启动：{error:#}")) };
                let _ = sender.try_send(event); shared.notify(); paused = true;
            }
            did_work = true;
        }
        if let Some(db) = &mut database {
            let tick = (|| -> Result<()> {
                if let Some(query) = active.as_mut().filter(|q| shared.current(q.generation)) {
                    let query_budget = Instant::now();
                    for _ in 0..8 {
                        if query.error.is_some() || query.truncated || query.cursor >= query.max_id || !shared.current(query.generation) { break; }
                        query_page(db, query, &roots, &shared)?; did_work = true;
                        if query_budget.elapsed() >= Duration::from_millis(4) { break; }
                    }
                }
                if db.stats.scanning && !paused {
                    scan_batch(db, &mut current, &mut active, &roots, &shared)?; did_work = true;
                }
                Ok(())
            })();
            if let Err(error) = tick {
                paused = true; current = None; db.stats.scanning = false;
                if let Some(query) = &mut active { query.error = Some(format!("索引更新已停止，结果尚未覆盖全部位置：{error:#}")); }
            }
            if let Some(query) = active.as_mut().filter(|q| shared.current(q.generation)) {
                // A repeated query can reuse the completed database pass while the
                // background filesystem pass continues to validate and add names.
                if (query.cursor >= query.max_id || query.truncated) && query.error.is_none() {
                    if let Some(cached) = cache.as_mut().filter(|cached| cached.query == query.query) {
                        if cached.paths.len() != query.paths.len() || cached.truncated != query.truncated {
                            cached.paths = query.paths.clone(); cached.truncated = query.truncated;
                        }
                        cached.revision = db.revision;
                    } else {
                        cache = Some(CachedQuery { query: query.query.clone(), revision: db.revision, paths: query.paths.clone(), truncated: query.truncated });
                    }
                }
                publish(query, db.stats, &sender, &shared);
            }
        }
        if !did_work {
            let guard = shared.control.lock().unwrap_or_else(|e| e.into_inner());
            if guard.request.is_none() && guard.action == ScanAction::None && !shared.stop.load(Ordering::Acquire) {
                if active.as_ref().is_some_and(|query| !query.final_sent) {
                    drop(shared.wake.wait_timeout(guard, Duration::from_millis(40)));
                } else { drop(shared.wake.wait(guard)); }
            }
        }
    }
    drop(current);
    if let Some(database) = database { let _ = database.persist_progress(); database.checkpoint(); }
}

fn query_page(db: &mut Database, query: &mut QueryWork, roots: &[PathBuf], shared: &Shared) -> Result<()> {
    let end = (query.cursor + QUERY_ID_WINDOW).min(query.max_id);
    let mut stale = Vec::new();
    {
        let mut statement = db.connection.prepare_cached("SELECT e.id,d.path,e.name FROM entries e JOIN directories d ON d.id=e.directory_id
            WHERE e.id>?1 AND e.id<=?2 AND instr(e.folded,?3)>0 ORDER BY e.id")?;
        let mut rows = statement.query(params![query.cursor, end, query.query])?;
        while let Some(row) = rows.next()? {
            if !shared.current(query.generation) { return Ok(()); }
            let path = PathBuf::from(decode_os(&row.get::<_, Vec<u8>>(1)?)?).join(decode_os(&row.get::<_, Vec<u8>>(2)?)?);
            if containing_root(&path, roots).is_none() { continue; }
            match existing_in_scope(&path, roots) {
                Ok(true) => query.add(path),
                Ok(false) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => stale.push(row.get::<_, i64>(0)?),
                Err(_) => db.stats.denied += 1,
            }
            if query.truncated { break; }
        }
    }
    for id in stale {
        let removed = db.connection.execute("DELETE FROM entries WHERE id=?1", [id])?;
        db.stats.indexed = db.stats.indexed.saturating_sub(removed as u64); db.revision += removed as u64;
    }
    query.cursor = end;
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
                    if containing_root(&path, roots).is_some() && fs::symlink_metadata(&path).is_ok() { query.add(path); }
                    if query.truncated { break; }
                }
            }
        }
    }
    if exhausted { db.finish_directory(directory)?; *current = None; }
    Ok(())
}

fn publish(query: &mut QueryWork, stats: Stats, sender: &SyncSender<SearchEvent>, shared: &Shared) -> bool {
    if !shared.current(query.generation) { return false; }
    if query.final_sent && (stats == query.last_stats || (stats.scanning && query.last_progress.elapsed() < Duration::from_millis(500))) { return false; }
    let end = (query.delivered + EVENT_BATCH).min(query.paths.len());
    let data = end > query.delivered;
    let finished = query.done(stats) && end == query.paths.len();
    if !data && !finished && query.last_progress.elapsed() < Duration::from_millis(500) { return false; }
    let paths = query.paths[query.delivered..end].iter().filter(|p| fs::symlink_metadata(p).is_ok()).cloned().collect();
    let event = query.event(stats, paths, finished);
    match sender.try_send(event) {
        Ok(()) => { query.delivered = end; query.final_sent = finished; query.last_stats = stats; query.last_progress = Instant::now(); shared.notify(); true }
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
            let root = std::env::temp_dir().join(format!("zsdesk-file-index-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
            fs::create_dir(&root).unwrap(); Self(root)
        }
        fn service(&self) -> FastSearch {
            FastSearch::with_roots(self.0.join("index/search-index.sqlite"), HWND::default(), 0, Some(vec![self.0.join("files")])).unwrap()
        }
    }
    impl Drop for Fixture { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); } }
    fn collect(service: &FastSearch, generation: u64) -> (HashSet<PathBuf>, SearchEvent) {
        let start = Instant::now(); let mut paths = HashSet::new();
        while start.elapsed() < Duration::from_secs(20) {
            for event in service.poll() {
                assert_eq!(event.generation, generation); assert!(event.paths.len() <= EVENT_BATCH);
                assert!(event.error.is_none(), "{:?}", event.error);
                paths.extend(event.paths.iter().cloned());
                if event.finished { return (paths, event); }
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("file-name index fixture timed out");
    }
    fn stop(mut service: FastSearch) {
        let control = service.shared.control.lock().unwrap_or_else(|e| e.into_inner());
        service.shared.stop.store(true, Ordering::Release);
        drop(control); service.shared.wake.notify_all();
        if let Some(worker) = service.worker.take() { worker.join().unwrap(); }
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
