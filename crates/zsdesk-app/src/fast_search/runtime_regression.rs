//! Explicit opt-in bounded native validation; never opens a user's search index.
use super::*;
use anyhow::Context;
use serde_json::json;
use std::{fs, os::windows::fs::MetadataExt, os::windows::ffi::OsStrExt};
use windows::{core::PCWSTR, Win32::{Foundation::CloseHandle, Storage::FileSystem::{
    CreateFileW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_READ_ATTRIBUTES,
    FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_SHARE_DELETE, OPEN_EXISTING,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
}}};

fn identity(path:&Path)->Result<(u32,u64,u32)>{
    let text:Vec<u16>=path.as_os_str().encode_wide().chain(Some(0)).collect();
    let handle=unsafe{CreateFileW(PCWSTR(text.as_ptr()),FILE_READ_ATTRIBUTES.0,
        FILE_SHARE_READ|FILE_SHARE_WRITE|FILE_SHARE_DELETE,None,OPEN_EXISTING,
        FILE_FLAG_BACKUP_SEMANTICS|FILE_FLAG_OPEN_REPARSE_POINT,None)}?;
    let mut info=BY_HANDLE_FILE_INFORMATION::default();
    let result=unsafe{GetFileInformationByHandle(handle,&mut info)};
    let _=unsafe{CloseHandle(handle)};result?;
    ensure!(info.dwFileAttributes&ntfs::REPARSE==0,"fixture must not be a reparse point");
    Ok((info.dwVolumeSerialNumber,(u64::from(info.nFileIndexHigh)<<32)|u64::from(info.nFileIndexLow),info.dwFileAttributes))
}
fn read_until(volume:&mut Volume,db:&mut Connection,state:&mut State,target:i64,tracked:&HashSet<u64>,
    pages:&mut usize,records:&mut usize,stop:&AtomicBool)->Result<()> {
    while state.checkpoint<target{
        ensure!(*pages<4,"bounded USN budget exhausted; no full journal scan was attempted");
        let mut page=volume.changes(state.checkpoint,state.journal,stop)?;*pages+=1;*records+=page.records.len();
        ensure!(page.next as i64>state.checkpoint,"journal did not advance");
        page.records.retain(|record|tracked.contains(&record.id));
        change_page(db,"fixture",state,&page)?;
    }
    Ok(())
}

#[test]
#[ignore="Requires ZSDESK_NATIVE_TEST_OUTPUT on NTFS; at most 2 MFT and 4 USN pages; direct fixture FS only"]
fn bounded_native_snapshot_journal_and_checkpoint()->Result<()>{
    let output=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("set an isolated ZSDESK_NATIVE_TEST_OUTPUT")?);
    ensure!(output.is_absolute(),"test output must be absolute");fs::create_dir_all(&output)?;
    ensure!(super::super::ordinary_directory_chain(&output)?,"test output has a reparse ancestor");
    let output=fs::canonicalize(output)?;
    let fixture=output.join(format!("native-fixture-{}",std::process::id()));fs::create_dir(&fixture)?;
    let fixture_identity=identity(&fixture)?;
    let report_path=output.join("native.current.json");
    if report_path.exists(){let old:serde_json::Value=serde_json::from_slice(&fs::read(&report_path)?)?;
        ensure!(old["schema"]=="ZSDesk.NativeSearchProbe.v1","foreign report file");}
    let started=Instant::now();
    let result=(||->Result<serde_json::Value>{
        let drive=match fixture.components().next(){Some(std::path::Component::Prefix(p))=>match p.kind(){
            std::path::Prefix::Disk(letter)|std::path::Prefix::VerbatimDisk(letter)=>letter as char,
            _=>anyhow::bail!("fixture must be on a local NTFS drive"),},_=>anyhow::bail!("missing drive")};
        let root=PathBuf::from(format!("{}:\\",drive.to_ascii_uppercase()));
        let stop=AtomicBool::new(false);let mut volume=Volume::open(&root)?;
        ensure!(volume.serial==fixture_identity.0,"fixture volume changed");
        let journal=volume.journal(&stop)?;
        let db_path=fixture.join("probe.sqlite");let mut db=Connection::open(&db_path)?;
        db.execute_batch("PRAGMA page_size=4096;PRAGMA cache_size=-1024;PRAGMA mmap_size=0;
            PRAGMA journal_mode=WAL;PRAGMA synchronous=FULL;PRAGMA max_page_count=4096;PRAGMA wal_autocheckpoint=128;")?;
        schema(&db)?;
        let mut snapshot=begin_snapshot(&mut db,"prefix",&root,volume.serial,volume.root_id,journal)?;
        let mut enum_records=0usize;let mut enum_pages=0usize;let mut enum_micros=0u128;let mut store_micros=0u128;
        for _ in 0..2{
            let call=Instant::now();let page=volume.enumerate(snapshot.cursor,&stop)?;enum_micros+=call.elapsed().as_micros();
            let Some(page)=page else{break;};enum_pages+=1;enum_records+=page.records.len();
            let store=Instant::now();snapshot_page(&mut db,"prefix",&mut snapshot,&page)?;store_micros+=store.elapsed().as_micros();
        }
        ensure!(enum_pages>0 && enum_records>0,"no MFT metadata records returned");
        let saved_cursor=snapshot.cursor;drop(db);
        let mut db=Connection::open(&db_path)?;
        let reopened=state(&db,"prefix")?.context("snapshot checkpoint missing")?;
        ensure!(reopened.cursor==saved_cursor && reopened.phase==SNAPSHOT,"partial snapshot was published or cursor was not durable");
        let cancelled=AtomicBool::new(true);let cancel_at=Instant::now();
        ensure!(volume.enumerate(saved_cursor,&cancelled).is_err(),"pre-cancelled enumeration performed work");
        let cancellation_micros=cancel_at.elapsed().as_micros();
        ensure!(cancel_at.elapsed()<Duration::from_millis(250),"pre-cancel was not bounded");

        let moving=fixture.join("moving");fs::create_dir(&moving)?;
        let original=moving.join("marker-中文, 空格.txt");fs::write(&original,b"owned NTFS regression fixture")?;
        let dir_id=identity(&moving)?.1;let file_id=identity(&original)?.1;
        let before=volume.journal(&stop)?;
        let mut graph=begin_snapshot(&mut db,"fixture",&fixture,volume.serial,fixture_identity.1,before)?;
        snapshot_page(&mut db,"fixture",&mut graph,&Page{next:1,records:vec![
            Record{id:dir_id,parent:fixture_identity.1,usn:0,reason:0,attributes:ntfs::DIRECTORY,name:"moving".into()},
            Record{id:file_id,parent:dir_id,usn:0,reason:0,attributes:0,name:original.file_name().unwrap().to_owned()},
        ]})?;
        // This is a fully known fixture subtree, not a completed volume snapshot.
        db.execute("UPDATE ntfs_volumes SET phase=4 WHERE volume='fixture'",[])?;graph.phase=READY;
        let renamed=moving.join("renamed-中文, 空格.txt");fs::rename(&original,&renamed)?;
        let moved=fixture.join("moved");fs::rename(&moving,&moved)?;
        let final_path=moved.join(renamed.file_name().unwrap());
        let tracked=HashSet::from([dir_id,file_id]);let mut pages=0;let mut records=0;
        let read_at=Instant::now();let target=volume.journal(&stop)?.next;
        read_until(&mut volume,&mut db,&mut graph,target,&tracked,&mut pages,&mut records,&stop)?;
        ensure!(resolve_path(&db,"fixture",file_id)?==Some(final_path.clone()),"native rename/parent recovery disagrees with the fixture");
        let alias=fixture.join("hardlink-中文.txt");fs::hard_link(&final_path,&alias)?;
        ensure!(identity(&alias)?.1==file_id,"hardlink identity differs");
        let target=volume.journal(&stop)?.next;
        read_until(&mut volume,&mut db,&mut graph,target,&tracked,&mut pages,&mut records,&stop)?;
        ensure!(meta(&db,"directory_repair")?==1,"hardlink/rename coverage obligation was not durable");
        fs::remove_file(&alias)?;fs::remove_file(&final_path)?;fs::remove_dir(&moved)?;
        let target=volume.journal(&stop)?.next;
        read_until(&mut volume,&mut db,&mut graph,target,&tracked,&mut pages,&mut records,&stop)?;
        ensure!(resolve_path(&db,"fixture",file_id)?.is_none(),"deleted fixture path remained reachable");
        let checkpoint=graph.checkpoint;let read_micros=read_at.elapsed().as_micros();
        db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;drop(db);
        let db=Connection::open(&db_path)?;
        ensure!(state(&db,"fixture")?.context("journal checkpoint missing")?.checkpoint==checkpoint,"journal checkpoint was not durable");
        let after=volume.journal(&stop)?;
        ensure!(after.id==journal.id,"journal identity changed externally during probe");
        let bytes=fs::read_dir(&fixture)?.filter_map(|entry|entry.ok()).filter_map(|entry|entry.metadata().ok()).filter(|m|m.is_file()).map(|m|m.len()).sum::<u64>();
        ensure!(bytes<32*1024*1024,"fixture storage budget exceeded");
        Ok(json!({"schema":"ZSDesk.NativeSearchProbe.v1","state":"passed","pid":std::process::id(),"volume":root,
            "enum_pages":enum_pages,"enum_records":enum_records,"enum_buffer_upper_bound_bytes":enum_pages*262144,
            "enum_and_parse_microseconds":enum_micros,"snapshot_sqlite_microseconds":store_micros,
            "usn_pages":pages,"usn_records_read":records,"usn_fixture_cycle_microseconds":read_micros,
            "snapshot_cursor":format!("{saved_cursor:016x}"),"checkpoint":checkpoint,"journal_id":format!("{:016x}",after.id),
            "cancel_microseconds":cancellation_micros,"cancellation_scope":"pre-cancel and batch boundary; no forced pending-driver stall",
            "rename_and_parent_path_verified":true,"delete_verified":true,"checkpoint_reopened":true,
            "partial_volume_snapshot_not_published":true,"journal_configuration_written":false,"shell_file_operations":false,"undo_executed":false,
            "index_isolated":true,"full_volume_enumeration":false,"not_a_full_index_benchmark":true,"fixture_bytes":bytes}))
    })();
    let cleanup=(||->Result<()>{ensure!(identity(&fixture)?==fixture_identity,"fixture directory identity changed");
        ensure!(fixture.parent()==Some(output.as_path()) && fs::symlink_metadata(&fixture)?.file_attributes()&ntfs::REPARSE==0,"cleanup boundary changed");
        fs::remove_dir_all(&fixture)?;Ok(())})();
    let mut report=match &result{Ok(value)=>value.clone(),Err(error)=>json!({"schema":"ZSDesk.NativeSearchProbe.v1","state":"failed","error":format!("{error:#}")})};
    report["cleanup_ok"]=json!(cleanup.is_ok());report["elapsed_ms"]=json!(started.elapsed().as_millis());
    if let Err(error)=&cleanup{report["cleanup_error"]=json!(format!("{error:#}"));report["state"]=json!("failed");}
    fs::write(report_path,serde_json::to_vec_pretty(&report)?)?;println!("{report}");
    result?;cleanup?;Ok(())
}
