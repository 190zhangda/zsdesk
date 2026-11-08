//! Persistent FRN graph and transactional USN checkpoints.
//! Directory scanning remains the coverage source for hard-link aliases and fallback.

use std::{collections::HashSet, ffi::OsString, path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering}, time::{Duration, Instant}};
use anyhow::{ensure, Result};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use super::{BackendKind, BackendPreference, IndexPhase, VolumeStatus, decode_os, encode_os, path_key, meta, set_meta};
use super::ntfs::{self, Journal, Page, Record, Volume};

const SNAPSHOT: i64 = 1;
const PRUNE: i64 = 2;
const CATCH_UP: i64 = 3;
const READY: i64 = 4;
const MAINTENANCE_BATCH: usize = 256;
const POLL_INTERVAL: Duration = Duration::from_secs(2);

pub fn schema(connection: &Connection) -> Result<()> {
    connection.execute_batch("CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY,value INTEGER NOT NULL) WITHOUT ROWID;
        CREATE TABLE IF NOT EXISTS ntfs_volumes(
        volume TEXT PRIMARY KEY,root BLOB NOT NULL,serial INTEGER NOT NULL,root_frn BLOB NOT NULL,
        journal BLOB NOT NULL,start_usn INTEGER NOT NULL,checkpoint INTEGER NOT NULL,
        cursor BLOB NOT NULL,epoch INTEGER NOT NULL,phase INTEGER NOT NULL,target_usn INTEGER NOT NULL,
        available INTEGER NOT NULL DEFAULT 0) WITHOUT ROWID;
        CREATE TABLE IF NOT EXISTS ntfs_nodes(id INTEGER PRIMARY KEY,volume TEXT NOT NULL,
        frn BLOB NOT NULL,parent BLOB NOT NULL,name BLOB NOT NULL,folded TEXT NOT NULL,
        attributes INTEGER NOT NULL,usn INTEGER NOT NULL,seen INTEGER NOT NULL,UNIQUE(volume,frn));
        CREATE INDEX IF NOT EXISTS ntfs_nodes_parent ON ntfs_nodes(volume,parent);
        CREATE TABLE IF NOT EXISTS ntfs_gc(volume TEXT NOT NULL,frn BLOB NOT NULL,PRIMARY KEY(volume,frn)) WITHOUT ROWID;")?;
    Ok(())
}
#[cfg(test)]
pub fn clear(connection: &Connection) -> Result<()> {
    connection.execute_batch("DELETE FROM ntfs_nodes; DELETE FROM ntfs_gc; DELETE FROM ntfs_volumes;")?;
    Ok(())
}
fn id(value: u64) -> Vec<u8> { value.to_le_bytes().to_vec() }
fn decode_id(value: Vec<u8>) -> Result<u64> {
    Ok(u64::from_le_bytes(value.try_into().map_err(|_| anyhow::anyhow!("索引文件标识长度无效"))?))
}

#[derive(Clone, Debug)]
struct State {
    serial: u32, root_id: u64, journal: u64, start: i64, checkpoint: i64,
    cursor: u64, epoch: i64, phase: i64, target: i64,
}
impl State {
    fn resumable(&self, serial: u32, root_id: u64, journal: Journal) -> bool {
        self.serial == serial && self.root_id == root_id
            && journal.contains(self.journal, if self.phase <= PRUNE { self.start } else { self.checkpoint })
    }
}
fn state(connection: &Connection, key: &str) -> Result<Option<State>> {
    type Row = (i64, Vec<u8>, Vec<u8>, i64, i64, Vec<u8>, i64, i64, i64);
    let row: Option<Row> = connection.query_row("SELECT serial,root_frn,journal,start_usn,checkpoint,cursor,epoch,phase,target_usn
        FROM ntfs_volumes WHERE volume=?1", [key], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?,r.get(8)?))).optional()?;
    row.map(|(serial, root, journal, start, checkpoint, cursor, epoch, phase, target)| {
        ensure!((0..=u32::MAX as i64).contains(&serial) && (SNAPSHOT..=READY).contains(&phase)
            && epoch > 0 && start >= 0 && checkpoint >= 0 && target >= 0, "NTFS 索引状态无效");
        Ok(State { serial: serial as u32, root_id: decode_id(root)?, journal: decode_id(journal)?, start,
            checkpoint, cursor: decode_id(cursor)?, epoch, phase, target })
    }).transpose()
}

fn begin_snapshot(connection: &mut Connection, key: &str, root: &Path, serial: u32, root_id: u64, journal: Journal) -> Result<State> {
    let old_epoch: i64 = connection.query_row("SELECT epoch FROM ntfs_volumes WHERE volume=?1", [key], |r| r.get(0)).optional()?.unwrap_or(0);
    let epoch = old_epoch.max(meta(connection, "ntfs_snapshot_epoch")?).checked_add(1).filter(|v| *v > 0)
        .ok_or_else(|| anyhow::anyhow!("索引代次超出范围，请重建索引"))?;
    let transaction = connection.transaction()?;
    set_meta(&transaction, "ntfs_snapshot_epoch", epoch)?;
    transaction.execute("INSERT INTO ntfs_volumes(volume,root,serial,root_frn,journal,start_usn,checkpoint,cursor,epoch,phase,target_usn,available)
        VALUES(?1,?2,?3,?4,?5,?6,?6,?7,?8,1,?6,1) ON CONFLICT(volume) DO UPDATE SET
        root=excluded.root,serial=excluded.serial,root_frn=excluded.root_frn,journal=excluded.journal,
        start_usn=excluded.start_usn,checkpoint=excluded.checkpoint,cursor=excluded.cursor,
        epoch=excluded.epoch,phase=1,target_usn=excluded.target_usn,available=1",
        params![key, encode_os(root.as_os_str()), i64::from(serial), id(root_id), id(journal.id), journal.next, id(0), epoch])?;
    transaction.execute("DELETE FROM ntfs_gc WHERE volume=?1", [key])?;
    if old_epoch > 0 { set_meta(&transaction, "directory_repair", 1)?; }
    transaction.commit()?;
    Ok(State { serial, root_id, journal: journal.id, start: journal.next, checkpoint: journal.next,
        cursor: 0, epoch, phase: SNAPSHOT, target: journal.next })
}

fn store_record(transaction: &Transaction<'_>, key: &str, state: &State, record: &Record, snapshot: bool) -> Result<i64> {
    if record.id == state.root_id { return Ok(0); }
    ensure!(record.name != "." && record.name != ".." && record.id != record.parent, "NTFS 文件父节点无效");
    if !snapshot && record.reason & ntfs::FILE_DELETE != 0 {
        let deleted = transaction.execute("DELETE FROM ntfs_nodes WHERE volume=?1 AND frn=?2 AND usn<=?3",
            params![key, id(record.id), record.usn])?;
        if deleted > 0 && record.attributes & ntfs::DIRECTORY != 0 {
            transaction.execute("INSERT OR IGNORE INTO ntfs_gc(volume,frn) VALUES(?1,?2)", params![key, id(record.id)])?;
        }
        return Ok(-(deleted as i64));
    }
    if !snapshot && record.reason & ntfs::RENAME_OLD != 0 && record.reason & ntfs::RENAME_NEW == 0 { return Ok(0); }
    let inserted = transaction.execute("INSERT OR IGNORE INTO ntfs_nodes(volume,frn,parent,name,folded,attributes,usn,seen)
        VALUES(?1,?2,?3,?4,?5,?6,?7,?8)", params![key,id(record.id),id(record.parent),encode_os(&record.name),
            record.name.to_string_lossy().to_lowercase(),i64::from(record.attributes),record.usn,state.epoch])?;
    if inserted == 0 {
        transaction.execute("UPDATE ntfs_nodes SET parent=?3,name=?4,folded=?5,attributes=?6,usn=?7,seen=?8
            WHERE volume=?1 AND frn=?2 AND (?9 OR usn<=?7)", params![key,id(record.id),id(record.parent),encode_os(&record.name),
            record.name.to_string_lossy().to_lowercase(),i64::from(record.attributes),record.usn,state.epoch,snapshot])?;
    }
    Ok(inserted as i64)
}

fn snapshot_page(connection: &mut Connection, key: &str, state: &mut State, page: &Page) -> Result<i64> {
    ensure!(page.next > state.cursor, "NTFS 快照游标未前进");
    let transaction = connection.transaction()?;
    let mut delta = 0;
    for record in &page.records { delta += store_record(&transaction, key, state, record, true)?; }
    transaction.execute("UPDATE ntfs_volumes SET cursor=?2 WHERE volume=?1", params![key,id(page.next)])?;
    transaction.commit()?;
    state.cursor = page.next;
    Ok(delta)
}

fn requires_directory_repair(record: &Record) -> bool {
    record.reason & (ntfs::HARD_LINK_CHANGE | ntfs::RENAME_OLD | ntfs::RENAME_NEW) != 0 ||
        (record.attributes & ntfs::DIRECTORY != 0 && record.reason & ntfs::FILE_DELETE != 0)
}
fn change_page(connection: &mut Connection, key: &str, state: &mut State, page: &Page) -> Result<(i64, bool)> {
    ensure!(page.next <= i64::MAX as u64 && page.next as i64 >= state.checkpoint
        && page.records.iter().all(|r| r.usn >= state.checkpoint && r.usn < page.next as i64), "USN 增量边界无效");
    let transaction = connection.transaction()?;
    let mut delta = 0;
    let mut repair = false;
    for record in &page.records {
        repair |= requires_directory_repair(record);
        delta += store_record(&transaction, key, state, record, false)?;
    }
    // The alias-repair obligation commits with the journal cursor. A crash
    // must not consume HARD_LINK_CHANGE and forget the required directory pass.
    if repair { set_meta(&transaction, "directory_repair", 1)?; }
    transaction.execute("UPDATE ntfs_volumes SET checkpoint=?2 WHERE volume=?1", params![key,page.next as i64])?;
    transaction.commit()?;
    state.checkpoint = page.next as i64;
    Ok((delta, repair))
}

/// A deleted directory makes descendants unresolvable immediately. Reclaim its
/// persistent subtree a bounded batch at a time, without a large recursive SQL.
fn collect_deleted(connection: &mut Connection, key: &str) -> Result<u64> {
    let parent: Option<Vec<u8>> = connection.query_row("SELECT frn FROM ntfs_gc WHERE volume=?1 LIMIT 1", [key], |r| r.get(0)).optional()?;
    let Some(parent) = parent else { return Ok(0) };
    let parent_live: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM ntfs_nodes WHERE volume=?1 AND frn=?2)", params![key,parent], |r|r.get(0))?;
    if parent_live {
        // Even the 16-bit NTFS sequence can eventually wrap. A live parent wins
        // over an old cleanup obligation; never remove a recreated subtree.
        connection.execute("DELETE FROM ntfs_gc WHERE volume=?1 AND frn=?2", params![key,parent])?;
        return Ok(0);
    }
    let children: Vec<(Vec<u8>, i64)> = {
        let mut statement = connection.prepare("SELECT frn,attributes FROM ntfs_nodes WHERE volume=?1 AND parent=?2 LIMIT ?3")?;
        let rows = statement.query_map(params![key,parent,MAINTENANCE_BATCH as i64], |r| Ok((r.get(0)?,r.get(1)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let transaction = connection.transaction()?;
    let mut deleted = 0;
    for (child, attributes) in &children {
        if *attributes & i64::from(ntfs::DIRECTORY) != 0 {
            transaction.execute("INSERT OR IGNORE INTO ntfs_gc(volume,frn) VALUES(?1,?2)", params![key,child])?;
        }
        deleted += transaction.execute("DELETE FROM ntfs_nodes WHERE volume=?1 AND frn=?2 AND parent=?3", params![key,child,parent])?;
    }
    if children.len() < MAINTENANCE_BATCH { transaction.execute("DELETE FROM ntfs_gc WHERE volume=?1 AND frn=?2", params![key,parent])?; }
    transaction.commit()?;
    Ok(deleted as u64)
}

/// Resolve from current directory FRNs, so a parent rename changes all descendant
/// paths without rewriting a million stored strings. Cycles and reparse parents
/// are rejected; file-reference sequence bits are never discarded.
pub fn resolve_path(connection: &Connection, volume: &str, file_id: u64) -> Result<Option<PathBuf>> {
    let root: Option<(Vec<u8>,Vec<u8>)> = connection.query_row("SELECT root,root_frn FROM ntfs_volumes WHERE volume=?1 AND available=1 AND phase>=3 AND length(root)<=65534",
        [volume], |r| Ok((r.get(0)?,r.get(1)?))).optional()?;
    let Some((root, root_id)) = root else { return Ok(None) };
    let root_id = decode_id(root_id)?;
    let mut current = file_id;
    let mut seen = HashSet::new();
    let mut parts = Vec::<OsString>::new();
    let mut bytes = root.len();
    while current != root_id {
        if parts.len() >= 512 || !seen.insert(current) { return Ok(None); }
        let node: Option<(Vec<u8>,Vec<u8>,i64)> = connection.query_row("SELECT parent,name,attributes FROM ntfs_nodes WHERE volume=?1 AND frn=?2 AND length(name)<=510",
            params![volume,id(current)], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        let Some((parent, name, attributes)) = node else { return Ok(None) };
        if !parts.is_empty() && (attributes & i64::from(ntfs::DIRECTORY) == 0 || attributes & i64::from(ntfs::REPARSE) != 0) { return Ok(None); }
        bytes += name.len() + 2;
        if bytes > 65534 { return Ok(None); }
        let name = decode_os(&name)?;
        if name == "." || name == ".." || name.is_empty() { return Ok(None); }
        let mut components = Path::new(&name).components();
        if !matches!(components.next(), Some(std::path::Component::Normal(_))) || components.next().is_some() { return Ok(None); }
        parts.push(name); current = decode_id(parent)?;
    }
    let mut path = PathBuf::from(decode_os(&root)?);
    for part in parts.into_iter().rev() { path.push(part); }
    Ok(Some(path))
}

struct Work { volume: Volume, key: String, state: State, count: u64, last_poll: Instant, dirty: bool }
pub struct Engine { work: Vec<Option<Work>>, reports: Vec<VolumeStatus>, turn: usize, pub scanned: u64 }
#[derive(Default)]
pub struct Update { pub worked: bool, pub changed: bool, pub repair_directories: bool }
impl Engine {
    pub fn new(connection: &mut Connection, roots: &[PathBuf], preference: BackendPreference, stop: &AtomicBool) -> Result<Self> {
        connection.execute("UPDATE ntfs_volumes SET available=0", [])?;
        let mut engine = Self { work: Vec::new(), reports: Vec::new(), turn: 0, scanned: 0 };
        for root in roots {
            ensure!(!stop.load(Ordering::Acquire), "索引已取消");
            let mut report = VolumeStatus { root: root.clone(), backend: BackendKind::DirectoryScan,
                phase: IndexPhase::Scanning, indexed: 0, checkpoint: None, warning: None };
            let attempt = (|| -> Result<Work> {
                ensure!(preference != BackendPreference::DirectoryScan, "已选择目录扫描");
                let mut volume = Volume::open(root)?;
                let journal = volume.journal(stop)?;
                let key = path_key(root).to_lowercase();
                let saved = state(connection, &key)?;
                let mut stored = match saved.filter(|s| s.resumable(volume.serial, volume.root_id, journal)) {
                    Some(state) => state,
                    None => begin_snapshot(connection, &key, root, volume.serial, volume.root_id, journal)?,
                };
                if stored.phase == READY && stored.checkpoint < journal.next {
                    stored.phase = CATCH_UP; stored.target = journal.next;
                    connection.execute("UPDATE ntfs_volumes SET phase=3,target_usn=?2 WHERE volume=?1", params![key, stored.target])?;
                }
                connection.execute("UPDATE ntfs_volumes SET available=1 WHERE volume=?1", [&key])?;
                let count = connection.query_row("SELECT count(*) FROM ntfs_nodes WHERE volume=?1", [&key], |r| r.get::<_,i64>(0))?.max(0) as u64;
                let dirty = stored.phase != READY;
                Ok(Work { volume, key, state: stored, count, last_poll: Instant::now() - POLL_INTERVAL, dirty })
            })();
            match attempt {
                Ok(work) => { report.backend = BackendKind::NativeNtfs; report.phase = phase(work.state.phase);
                    report.indexed = work.count; report.checkpoint = Some(work.state.checkpoint); engine.work.push(Some(work)); }
                Err(error) => { if preference != BackendPreference::DirectoryScan { report.warning = Some(format!("目录扫描回退：{error:#}")); }
                    engine.work.push(None); }
            }
            engine.reports.push(report);
        }
        Ok(engine)
    }

    pub fn statuses(&self, scanning: bool) -> Vec<VolumeStatus> {
        self.reports.iter().cloned().map(|mut r| { if r.backend == BackendKind::DirectoryScan {
            r.phase = if scanning { IndexPhase::Scanning } else { IndexPhase::Ready };
        } r }).collect()
    }
    pub fn busy(&self) -> bool { self.work.iter().flatten().any(|w| w.state.phase != READY) }
    pub fn active(&self) -> bool { self.work.iter().any(Option::is_some) }
    pub fn indexed(&self) -> u64 { self.work.iter().flatten().map(|w| w.count).sum() }
    pub fn stage(&self, scanning: bool) -> IndexPhase {
        if self.work.iter().flatten().any(|w| w.state.phase <= PRUNE) { IndexPhase::Snapshot }
        else if self.busy() { IndexPhase::CatchingUp }
        else if scanning { IndexPhase::Scanning } else { IndexPhase::Ready }
    }
    pub fn backend(&self) -> BackendKind { if self.active() { BackendKind::Mixed } else { BackendKind::DirectoryScan } }
    pub fn coverage_complete(&mut self) {
        for report in &mut self.reports {
            if report.warning.as_deref() == Some("目录名称或硬链接发生变化，正在补全路径")
                || (report.phase == IndexPhase::Ready && report.warning.as_deref() == Some("USN 日志范围已变化，正在重建元数据快照")) {
                report.warning = None;
            }
        }
    }

    pub fn tick(&mut self, connection: &mut Connection, stop: &AtomicBool) -> Result<Update> {
        if stop.load(Ordering::Acquire) { return Ok(Update::default()); }
        if self.work.is_empty() { return Ok(Update::default()); }
        let mut chosen = None;
        for _ in 0..self.work.len() {
            let index = self.turn % self.work.len(); self.turn = (index + 1) % self.work.len();
            if self.work[index].as_ref().is_some_and(|w| w.state.phase != READY || w.last_poll.elapsed() >= POLL_INTERVAL) {
                chosen = Some(index); break;
            }
        }
        let Some(index) = chosen else { return Ok(Update::default()) };
        let work = self.work[index].as_mut().unwrap();
        let update = tick_volume(connection, work, stop, &mut self.scanned);
        match update {
            Ok(update) => {
                self.reports[index].phase = phase(work.state.phase); self.reports[index].indexed = work.count;
                self.reports[index].checkpoint = Some(work.state.checkpoint);
                if update.repair_directories { self.reports[index].warning = Some("目录名称或硬链接发生变化，正在补全路径".into()); }
                Ok(update)
            }
            Err(mut error) => {
                if stop.load(Ordering::Acquire) { return Ok(Update::default()); }
                let journal_race = error.downcast_ref::<windows::core::Error>().is_some_and(|e|
                    matches!(e.code().0 as u32 & 0xFFFF, 87 | 1178 | 1181));
                if journal_race {
                    // A journal can roll between QUERY and READ. Reopen/re-query
                    // once; never replay a batch using an old journal identity.
                    match restart_snapshot(connection, work, stop, true) {
                        Ok(true) => {
                            self.reports[index].phase = IndexPhase::Snapshot;
                            self.reports[index].warning = Some("USN 日志范围已变化，正在重建元数据快照".into());
                            return Ok(Update { worked: true, changed: true, repair_directories: true });
                        }
                        Ok(false) => {},
                        Err(reopen) => error = reopen.context("重新读取 NTFS 卷状态失败"),
                    }
                }
                // Capacity/SQLite failures are surfaced to the owner; do not hide
                // a failed transaction as a successfully advanced checkpoint.
                if error.downcast_ref::<rusqlite::Error>().is_some() { return Err(error); }
                connection.execute("UPDATE ntfs_volumes SET available=0 WHERE volume=?1", [&work.key])?;
                set_meta(connection, "directory_repair", 1)?;
                self.reports[index].backend = BackendKind::DirectoryScan;
                self.reports[index].warning = Some(format!("NTFS 更新暂停，目录扫描回退：{error:#}"));
                self.work[index] = None;
                Ok(Update { worked: true, changed: true, repair_directories: true })
            }
        }
    }
}
fn phase(value: i64) -> IndexPhase { match value { SNAPSHOT | PRUNE => IndexPhase::Snapshot, CATCH_UP => IndexPhase::CatchingUp, _ => IndexPhase::Ready } }
fn delta(count: &mut u64, value: i64) { if value < 0 { *count = count.saturating_sub(value.unsigned_abs()); } else { *count = count.saturating_add(value as u64); } }
fn restart_snapshot(connection: &mut Connection, work: &mut Work, stop: &AtomicBool, only_if_changed: bool) -> Result<bool> {
    let mut volume = Volume::open(&work.volume.root)?;
    let journal = volume.journal(stop)?;
    if only_if_changed && work.state.resumable(volume.serial, volume.root_id, journal) { return Ok(false); }
    let state = begin_snapshot(connection, &work.key, &volume.root, volume.serial, volume.root_id, journal)?;
    work.volume = volume; work.state = state; work.dirty = true;
    Ok(true)
}

fn tick_volume(connection: &mut Connection, work: &mut Work, stop: &AtomicBool, scanned: &mut u64) -> Result<Update> {
    let mut update = Update { worked: true, ..Default::default() };
    if work.state.phase == SNAPSHOT {
        if let Some(page) = work.volume.enumerate(work.state.cursor, stop)? {
            delta(&mut work.count, snapshot_page(connection, &work.key, &mut work.state, &page)?);
            *scanned += page.records.len() as u64;
        } else {
            connection.execute("UPDATE ntfs_volumes SET phase=2 WHERE volume=?1", [&work.key])?; work.state.phase = PRUNE;
        }
        return Ok(update);
    }
    if work.state.phase == PRUNE {
        let deleted = connection.execute("DELETE FROM ntfs_nodes WHERE id IN (SELECT id FROM ntfs_nodes WHERE volume=?1 AND seen<>?2 LIMIT ?3)",
            params![work.key,work.state.epoch,MAINTENANCE_BATCH as i64])?;
        work.count = work.count.saturating_sub(deleted as u64);
        if deleted == 0 {
            let journal = work.volume.journal(stop)?;
            if !journal.contains(work.state.journal, work.state.start) {
                restart_snapshot(connection, work, stop, false)?;
                update.repair_directories = true;
            } else {
                connection.execute("UPDATE ntfs_volumes SET phase=3,checkpoint=start_usn,target_usn=?2 WHERE volume=?1", params![work.key,journal.next])?;
                work.state.phase = CATCH_UP; work.state.checkpoint = work.state.start; work.state.target = journal.next;
                update.changed = true;
            }
        }
        return Ok(update);
    }
    let journal = work.volume.journal(stop)?;
    work.last_poll = Instant::now();
    if !journal.contains(work.state.journal, work.state.checkpoint) {
        restart_snapshot(connection, work, stop, false)?;
        update.changed = true; update.repair_directories = true; return Ok(update);
    }
    if work.state.phase == READY {
        let deleted = collect_deleted(connection, &work.key)?;
        work.count = work.count.saturating_sub(deleted);
        if work.state.checkpoint == journal.next { update.worked = deleted > 0; return Ok(update); }
        connection.execute("UPDATE ntfs_volumes SET phase=3,target_usn=?2 WHERE volume=?1", params![work.key,journal.next])?;
        work.state.phase = CATCH_UP; work.state.target = journal.next;
        work.dirty = false;
    }
    if work.state.checkpoint < work.state.target {
        let page = work.volume.changes(work.state.checkpoint, work.state.journal, stop)?;
        ensure!(page.next as i64 > work.state.checkpoint, "USN 增量没有进展");
        let (added, repair) = change_page(connection, &work.key, &mut work.state, &page)?;
        delta(&mut work.count, added); update.repair_directories |= repair;
        work.dirty |= page.records.iter().any(|r| r.reason & (0x100 | ntfs::FILE_DELETE | ntfs::RENAME_OLD | ntfs::RENAME_NEW | ntfs::HARD_LINK_CHANGE | 0x100000) != 0);
        *scanned += page.records.len() as u64;
    }
    if work.state.checkpoint >= work.state.target {
        connection.execute("UPDATE ntfs_volumes SET phase=4 WHERE volume=?1", [&work.key])?;
        work.state.phase = READY; update.changed |= work.dirty; work.dirty = false;
    }
    Ok(update)
}

#[cfg(test)]
#[path="runtime_regression.rs"]
mod runtime_regression;

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (Connection, State) {
        let mut db = Connection::open_in_memory().unwrap(); schema(&db).unwrap();
        let state = begin_snapshot(&mut db,"c:\\",Path::new("C:\\"),1,5,Journal { id:7,first:0,next:10,lowest_valid:0 }).unwrap();
        (db,state)
    }
    fn record(id: u64, parent: u64, name: &str, directory: bool, usn: i64, reason: u32) -> Record {
        Record { id,parent,name:name.into(),attributes:if directory { ntfs::DIRECTORY } else { 0 },usn,reason }
    }
    fn ready(db: &Connection) { db.execute("UPDATE ntfs_volumes SET phase=4", []).unwrap(); }
    #[test]
    fn directory_move_resolves_descendants_and_deleted_subtree_is_unreachable() {
        let (mut db,mut state) = fixture();
        snapshot_page(&mut db,"c:\\",&mut state,&Page { next:100,records:vec![record(10,5,"目录甲",true,1,0),record(11,10,"子目录",true,2,0),record(12,11,"文件.txt",false,3,0)] }).unwrap(); ready(&db);
        assert_eq!(resolve_path(&db,"c:\\",12).unwrap().unwrap(),PathBuf::from(r"C:\目录甲\子目录\文件.txt"));
        change_page(&mut db,"c:\\",&mut state,&Page { next:22,records:vec![record(11,10,"子目录",true,20,ntfs::RENAME_OLD),record(11,5,"新名称",true,21,ntfs::RENAME_NEW)] }).unwrap();
        assert_eq!(resolve_path(&db,"c:\\",12).unwrap().unwrap(),PathBuf::from(r"C:\新名称\文件.txt"));
        change_page(&mut db,"c:\\",&mut state,&Page { next:24,records:vec![record(11,5,"新名称",true,23,ntfs::FILE_DELETE)] }).unwrap();
        assert!(resolve_path(&db,"c:\\",12).unwrap().is_none());
        assert_eq!(collect_deleted(&mut db,"c:\\").unwrap(),1);
        assert_eq!(db.query_row("SELECT count(*) FROM ntfs_nodes WHERE frn=?1",[id(12)],|r|r.get::<_,i64>(0)).unwrap(),0);
    }
    #[test]
    fn checkpoint_and_records_roll_back_together() {
        let (mut db,mut state) = fixture();
        db.execute_batch("CREATE TRIGGER reject_bad BEFORE INSERT ON ntfs_nodes WHEN NEW.folded='bad' BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
        let result = change_page(&mut db,"c:\\",&mut state,&Page { next:13,records:vec![record(10,5,"good",false,10,ntfs::HARD_LINK_CHANGE),record(11,5,"bad",false,12,0)] });
        assert!(result.is_err()); assert_eq!(state.checkpoint,10);
        assert_eq!(db.query_row("SELECT checkpoint FROM ntfs_volumes",[],|r|r.get::<_,i64>(0)).unwrap(),10);
        assert_eq!(db.query_row("SELECT count(*) FROM ntfs_nodes",[],|r|r.get::<_,i64>(0)).unwrap(),0);
        assert_eq!(super::super::meta(&db,"directory_repair").unwrap(),0);
    }
    #[test]
    fn hardlinks_require_directory_repair_and_reused_sequence_is_distinct() {
        let (mut db,mut state) = fixture();
        let old = 0x0001_0000_0000_0010; let new = 0x0002_0000_0000_0010;
        snapshot_page(&mut db,"c:\\",&mut state,&Page { next:100,records:vec![record(new,5,"new.txt",false,30,0)] }).unwrap(); ready(&db);
        let (_,repair) = change_page(&mut db,"c:\\",&mut state,&Page { next:32,records:vec![record(old,5,"old.txt",false,29,ntfs::FILE_DELETE),record(new,5,"new.txt",false,31,ntfs::HARD_LINK_CHANGE)] }).unwrap();
        assert!(repair); assert!(resolve_path(&db,"c:\\",new).unwrap().is_some());
        assert_eq!(db.query_row("SELECT value FROM meta WHERE key='directory_repair'",[],|r|r.get::<_,i64>(0)).unwrap(),1);
    }
    #[test]
    fn snapshot_newer_than_replayed_record_keeps_the_current_name() {
        let (mut db,mut state) = fixture();
        snapshot_page(&mut db,"c:\\",&mut state,&Page { next:100,records:vec![record(10,5,"current.txt",false,40,0)] }).unwrap();ready(&db);
        change_page(&mut db,"c:\\",&mut state,&Page { next:30,records:vec![record(10,5,"old.txt",false,20,ntfs::RENAME_NEW)] }).unwrap();
        assert_eq!(resolve_path(&db,"c:\\",10).unwrap().unwrap(),PathBuf::from(r"C:\current.txt"));
    }
    #[test]
    fn journal_replacement_or_wrap_requires_a_new_snapshot() {
        let (_db,mut state) = fixture();
        let journal = Journal { id:7,first:20,next:100,lowest_valid:0 };
        assert!(!state.resumable(1,5,journal));
        state.phase = READY; state.checkpoint = 80;
        assert!(state.resumable(1,5,journal));
        assert!(!state.resumable(2,5,journal)); assert!(!state.resumable(1,6,journal));
        assert!(!state.resumable(1,5,Journal { id:8,..journal }));
        state.checkpoint = 101; assert!(!state.resumable(1,5,journal));
    }
    #[test]
    fn interrupted_rebuild_cannot_reuse_an_old_snapshot_marker() {
        let (mut db,mut old) = fixture();
        snapshot_page(&mut db,"c:\\",&mut old,&Page {next:100,records:vec![record(10,5,"deleted.txt",false,1,0)]}).unwrap();
        db.execute("DELETE FROM ntfs_volumes",[]).unwrap();
        let next = begin_snapshot(&mut db,"c:\\",Path::new("C:\\"),1,5,Journal {id:7,first:0,next:20,lowest_valid:0}).unwrap();
        assert!(next.epoch>old.epoch);
        assert_eq!(db.execute("DELETE FROM ntfs_nodes WHERE seen<>?1",[next.epoch]).unwrap(),1);
        ready(&db); assert!(resolve_path(&db,"c:\\",10).unwrap().is_none());
    }
    #[test]
    fn stale_gc_does_not_remove_a_recreated_parent() {
        let (mut db,mut state) = fixture();
        snapshot_page(&mut db,"c:\\",&mut state,&Page {next:100,records:vec![record(10,5,"parent",true,1,0)]}).unwrap();ready(&db);
        change_page(&mut db,"c:\\",&mut state,&Page {next:22,records:vec![record(10,5,"parent",true,20,ntfs::FILE_DELETE),record(10,5,"recreated",true,21,0x100)]}).unwrap();
        change_page(&mut db,"c:\\",&mut state,&Page {next:24,records:vec![record(11,10,"new.txt",false,23,0x100)]}).unwrap();
        assert_eq!(collect_deleted(&mut db,"c:\\").unwrap(),0);
        assert_eq!(resolve_path(&db,"c:\\",11).unwrap().unwrap(),PathBuf::from(r"C:\recreated\new.txt"));
    }
    #[test]
    fn missing_cycles_and_reparse_ancestors_never_escape_the_root() {
        let (mut db,mut state) = fixture();
        let mut reparse = record(20,5,"junction",true,1,0);reparse.attributes|=ntfs::REPARSE;
        snapshot_page(&mut db,"c:\\",&mut state,&Page {next:100,records:vec![reparse,record(21,20,"outside.txt",false,2,0),record(30,31,"cycle-a",true,3,0),record(31,30,"cycle-b",true,4,0),record(40,999,"missing",false,5,0)]}).unwrap();ready(&db);
        for file in [21,30,40] { assert!(resolve_path(&db,"c:\\",file).unwrap().is_none()); }
    }
}
