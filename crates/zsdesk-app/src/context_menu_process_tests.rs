use super::*;
use std::{ffi::c_void,os::windows::io::AsRawHandle};
use zsdesk_windows::shell::menu::QuickMenu;
use windows::Win32::{System::{IO::DeviceIoControl,Ioctl::FSCTL_SET_SPARSE},Storage::FileSystem::GetCompressedFileSizeW};
#[link(name="user32")]unsafe extern "system"{
    fn CreateDesktopW(name:*const u16,device:*const u16,mode:*const c_void,flags:u32,access:u32,attributes:*const c_void)->*mut c_void;
    fn SetThreadDesktop(desktop:*mut c_void)->i32;
    fn CloseDesktop(desktop:*mut c_void)->i32;
}
thread_local!{static ENTERED:std::cell::Cell<bool>=const{std::cell::Cell::new(false)};}
unsafe extern "system" fn owner_proc(hwnd:HWND,msg:u32,wp:WPARAM,lp:LPARAM)->LRESULT{
    if msg==WM_ENTERMENULOOP{ENTERED.with(|value|value.set(true));unsafe{SetTimer(Some(hwnd),2,40,None);}}
    if msg==WM_TIMER&&wp.0==2{let _=unsafe{KillTimer(Some(hwnd),2)};let _=unsafe{EndMenu()};return LRESULT(0);}
    unsafe{DefWindowProcW(hwnd,msg,wp,lp)}
}
struct Owner(HWND);impl Drop for Owner{fn drop(&mut self){let _=unsafe{DestroyWindow(self.0)};}}
pub(crate) struct Environment{key:&'static str,previous:Option<std::ffi::OsString>}
impl Environment{pub(crate) fn set(key:&'static str,value:impl AsRef<std::ffi::OsStr>)->Self{let previous=std::env::var_os(key);std::env::set_var(key,value);Self{key,previous}}}
impl Drop for Environment{fn drop(&mut self){if let Some(previous)=&self.previous{std::env::set_var(self.key,previous);}else{std::env::remove_var(self.key);}}}
pub(crate) struct Fixtures(pub(crate) Vec<PathBuf>);impl Drop for Fixtures{fn drop(&mut self){for path in &self.0{let _=fs::remove_file(path);}}}
pub(crate) fn sparse(root:&Path)->Result<(Fixtures,Vec<Vec<u8>>,Vec<serde_json::Value>)>{
    let mut fixtures=Fixtures(Vec::new());let mut pidls=Vec::new();let mut rows=Vec::new();
    for (name,gib) in [("ZSDesk-menu-大文件.bin",12u64),("ZSDesk-menu-归档.zip",6),("ZSDesk-menu-视频.mp4",6)]{
        let path=root.join(name);let file=fs::OpenOptions::new().write(true).create_new(true).open(&path)?;fixtures.0.push(path.clone());
        let mut returned=0;unsafe{DeviceIoControl(HANDLE(file.as_raw_handle()),FSCTL_SET_SPARSE,None,0,None,0,Some(&mut returned),None)}?;
        let logical=gib*1024*1024*1024;file.set_len(logical)?;drop(file);
        let mut high=0;let low=unsafe{GetCompressedFileSizeW(PCWSTR(wide(&path.to_string_lossy()).as_ptr()),Some(&mut high))};let physical=(u64::from(high)<<32)|u64::from(low);
        ensure!(physical<1024*1024,"fixture unexpectedly allocated data blocks");
        pidls.push(zsdesk_windows::shell::pidl::location_pidl(&zsdesk_core::model::explorer::ShellLocation::Path(path.clone())).context("fixture PIDL")?);
        rows.push(serde_json::json!({"name":name,"logical_bytes":logical,"allocated_bytes":physical}));
    }Ok((fixtures,pidls,rows))
}
fn complete(client:&mut Client,owner:HWND)->Result<(std::result::Result<MenuOutcome,String>,(u64,u64))>{
    complete_with_guard(client,owner,&mut |_|true)
}
fn complete_with_guard(client:&mut Client,owner:HWND,before:&mut dyn FnMut(bool)->bool)->Result<(std::result::Result<MenuOutcome,String>,(u64,u64))>{
    let deadline=Instant::now()+Duration::from_secs(35);
    loop{
        ensure!(unsafe{IsWindow(Some(owner))}.as_bool(),"parent window was lost");
        if let Some(result)=client.poll(before){return Ok((result.map_err(|error|format!("{error:#}")),client.timings().unwrap_or_default()));}
        ensure!(Instant::now()<deadline,"worker test exceeded protocol timeout");pump(10);
    }
}

fn read_clipboard_retry(stage:&str)->Result<(windows::Win32::System::Com::IDataObject,u32)>{
    use windows::Win32::System::{DataExchange::GetClipboardSequenceNumber,Ole::OleGetClipboard};
    let deadline=Instant::now()+Duration::from_secs(1);
    loop{
        let before=unsafe{GetClipboardSequenceNumber()};
        match unsafe{OleGetClipboard()}{
            Ok(data) if before==unsafe{GetClipboardSequenceNumber()}=>return Ok((data,before)),
            Ok(_)=>ensure!(Instant::now()<deadline,"{stage}: clipboard changed while capturing a stable snapshot"),
            Err(error)=>{
                if error.code().0 as u32!=0x800401d0||Instant::now()>=deadline{return Err(error).with_context(||format!("{stage}: clipboard acquisition failed after bounded retry"));}
            }
        }
        pump(20);
    }
}
fn exercise(root:&Path,desktop_name:&str)->Result<serde_json::Value>{
    unsafe{OleInitialize(None)?;}
    struct Apartment;impl Drop for Apartment{fn drop(&mut self){unsafe{OleUninitialize();}}}let _apartment=Apartment;
    let _desktop=Environment::set("ZSDESK_MENU_TEST_DESKTOP",desktop_name);let _root=Environment::set("ZSDESK_MENU_ROOT",root.join("workers"));
    let (files,pidls,fixtures)=sparse(root)?;
    let module=HINSTANCE(unsafe{GetModuleHandleW(None)?}.0);let class=w!("ZSDesk.MenuIsolation.Parent");
    unsafe{RegisterClassW(&WNDCLASSW{hInstance:module,lpfnWndProc:Some(owner_proc),lpszClassName:class,..Default::default()});}
    let owner=Owner(unsafe{CreateWindowExW(WS_EX_NOACTIVATE,class,w!("Menu isolation test"),WS_OVERLAPPEDWINDOW,20,20,640,480,None,None,Some(module),None)}?);
    unsafe{let _=ShowWindow(owner.0,SW_SHOWNOACTIVATE);}
    let options=ContextMenuOptions{modern:false,..Default::default()};let point=POINT{x:70,y:100};
    let started=Instant::now();let quick=QuickMenu::new(pidls.len(),false,&options)?;let construction=started.elapsed().as_micros()as u64;
    let picked=quick.track_raw(owner.0,point);ensure!(picked.is_none()&&ENTERED.with(std::cell::Cell::get),"immediate popup did not open/dismiss");drop(quick);
    let mut workers=Vec::new();
    for probe in ["crash","memory-limit","dismiss","menu-dismiss"]{
        let _probe=Environment::set("ZSDESK_MENU_TEST_PROBE",probe);let mut client=Client::spawn(owner.0,point,pidls.clone(),None,options.clone(),MenuInterceptions::default(),Mode::More)?;
        let child=client.pid().context("worker pid")?;ensure!(child!=std::process::id(),"extension ran in parent");
        let mut limits=JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        unsafe{QueryInformationJobObject(Some(HANDLE(client.job.context("worker job")? as*mut _)),JobObjectExtendedLimitInformation,(&mut limits as*mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),std::mem::size_of_val(&limits)as u32,None)}?;
        ensure!(limits.ProcessMemoryLimit==WORKER_MEMORY_LIMIT&&limits.BasicLimitInformation.LimitFlags.contains(JOB_OBJECT_LIMIT_PROCESS_MEMORY|JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK|JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION)
            &&!limits.BasicLimitInformation.LimitFlags.contains(JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE),"worker resource/child lifetime policy changed");
        let(result,timing)=complete(&mut client,owner.0)?;
        if probe=="crash"{ensure!(result.as_ref().is_err_and(|error|error.to_ascii_lowercase().contains("c0000005")),"unhandled worker access violation was not detected: {result:?}");}else{ensure!(matches!(result,Ok(MenuOutcome::Dismissed)),"{probe} failed: {result:?}");}
        workers.push(serde_json::json!({"probe":probe,"pid":child,"outcome":result,"bind_microseconds":timing.0,"populate_microseconds":timing.1,"parent_alive":true,"memory_limit_bytes":limits.ProcessMemoryLimit,"child_process_breakaway":true,"kill_on_job_close":false}));drop(client);
    }
    let end=Instant::now()+Duration::from_secs(5);while pending_workers()>0&&Instant::now()<end{pump(10);}ensure!(pending_workers()==0,"menu worker did not exit");
    for (path,record) in files.0.iter().zip(&fixtures){ensure!(fs::metadata(path)?.len()==record["logical_bytes"].as_u64().unwrap(),"fixture content length changed");}
    drop(owner);drop(files);
    Ok(serde_json::json!({"fixtures":fixtures,"logical_total_bytes":24u64*1024*1024*1024,"quick_menu_construct_microseconds":construction,"quick_menu_native_popup":true,"workers":workers,"all_workers_exited":true,"fixtures_removed":true,"fault_injection":"unhandled noncontinuable SEH access violation 0xc0000005 before extension invocation"}))
}
#[test]
#[ignore="Private desktop, sparse 24 GiB selection and extension worker process isolation; requires ZSDESK_NATIVE_TEST_OUTPUT"]
fn native_large_selection_immediate_menu_and_extension_isolation()->Result<()>{
    let root=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("output")?).join("menu-isolation");ensure!(root.is_absolute(),"absolute output required");fs::create_dir_all(&root)?;
    let before=unsafe{GetForegroundWindow()};let name=format!("ZSDesk.MenuIsolation.{}",std::process::id());
    let desktop=unsafe{CreateDesktopW(wide(&name).as_ptr(),std::ptr::null(),std::ptr::null(),0,0x000f00ff,std::ptr::null())};ensure!(!desktop.is_null(),"private desktop failed");let raw=desktop as usize;let target=root.clone();
    let result=thread::spawn(move||->Result<serde_json::Value>{ensure!(unsafe{SetThreadDesktop(raw as*mut c_void)}!=0,"SetThreadDesktop failed");exercise(&target,&name)}).join().map_err(|_|anyhow::anyhow!("isolation test panicked")).and_then(|value|value);
    let closed=unsafe{CloseDesktop(desktop)}!=0;let foreground=unsafe{GetForegroundWindow()};let mut pid=0;unsafe{GetWindowThreadProcessId(foreground,Some(&mut pid));}
    let isolated=pid!=std::process::id();let report=serde_json::json!({"state":if result.is_ok()&&closed&&isolated{"passed"}else{"failed"},"checks":result.as_ref().map_err(|error|format!("{error:#}")),"desktop_closed":closed,"foreground_unchanged":before==foreground,"test_did_not_activate":isolated});
    fs::write(root.join("verification.current.json"),serde_json::to_vec_pretty(&report)?)?;println!("{report}");result?;ensure!(closed&&isolated,"desktop cleanup/focus changed");Ok(())
}

fn exercise_modern_owner(root:&Path,desktop_name:&str)->Result<serde_json::Value>{
    use windows::Win32::System::{DataExchange::GetClipboardSequenceNumber,Ole::OleSetClipboard};
    unsafe{OleInitialize(None)?;}
    struct Apartment;impl Drop for Apartment{fn drop(&mut self){unsafe{OleUninitialize();}}}let _apartment=Apartment;
    struct Clipboard{previous:windows::Win32::System::Com::IDataObject,ours:Option<u32>,child:Option<u32>}
    impl Clipboard{
        fn restore(&mut self)->Result<()>{
            let deadline=Instant::now()+Duration::from_secs(1);
            let result=loop{
                let before=unsafe{GetClipboardSequenceNumber()};
                let ours=self.ours==Some(before)||self.child.is_some_and(|child|unsafe{GetClipboardOwner()}.ok().is_some_and(|window|{
                    let mut pid=0;unsafe{GetWindowThreadProcessId(window,Some(&mut pid));}pid==child
                }));
                if !ours||before!=unsafe{GetClipboardSequenceNumber()}{break Ok(());}
                match unsafe{OleSetClipboard(&self.previous)}{
                    Ok(())=>break Ok(()),
                    Err(error) if error.code().0 as u32==0x800401d0&&Instant::now()<deadline=>pump(20),
                    Err(error)=>break Err(error).context("restore native-menu clipboard after bounded retry"),
                }
            };
            self.ours=None;self.child=None;
            result
        }
    }
    impl Drop for Clipboard{fn drop(&mut self){if let Err(error)=self.restore(){eprintln!("{error:#}");}}}
    let _desktop=Environment::set("ZSDESK_MENU_TEST_DESKTOP",desktop_name);
    let _root=Environment::set("ZSDESK_MENU_ROOT",root.join("workers"));
    let _probe=Environment::set("ZSDESK_MENU_TEST_PROBE","modern-owner-copy");
    let mut fixtures=Fixtures(Vec::new());let mut pidls=Vec::new();
    for name in ["原生菜单-甲.txt","原生菜单 乙.txt"]{
        let path=root.join(name);fs::write(&path,b"ZSDesk native owner command fixture")?;fixtures.0.push(path.clone());
        pidls.push(zsdesk_windows::shell::pidl::location_pidl(&zsdesk_core::model::explorer::ShellLocation::Path(path)).context("fixture PIDL")?);
    }
    let module=HINSTANCE(unsafe{GetModuleHandleW(None)?}.0);let class=w!("ZSDesk.ModernMenu.Parent");
    unsafe{RegisterClassW(&WNDCLASSW{hInstance:module,lpfnWndProc:Some(owner_proc),lpszClassName:class,..Default::default()});}
    let owner=Owner(unsafe{CreateWindowExW(WS_EX_NOACTIVATE,class,w!("Native menu owner regression"),WS_OVERLAPPEDWINDOW,20,20,640,480,None,None,Some(module),None)}?);
    unsafe{let _=ShowWindow(owner.0,SW_SHOWNOACTIVATE);}
    // A busy or concurrently changing clipboard declines the test before any
    // menu command is permitted; it is never treated as an empty clipboard.
    let(previous,initial_sequence)=read_clipboard_retry("preserve before native copy")?;
    let mut clipboard=Clipboard{previous,ours:None,child:None};
    let mut client=Client::spawn(owner.0,POINT{x:70,y:100},pidls,None,ContextMenuOptions{modern:true,..Default::default()},MenuInterceptions::default(),Mode::More)?;
    let child=client.pid().context("worker PID")?;
    clipboard.child=Some(child);
    let mut external_clipboard_change=false;
    let(result,timing)=match complete_with_guard(&mut client,owner.0,&mut |before_invoke|{
        if before_invoke&&unsafe{GetClipboardSequenceNumber()}!=initial_sequence{external_clipboard_change=true;return false;}true
    }){
        Ok(value)=>value,
        Err(error)=>{
            let stderr=worker_stderr(&client.workspace);
            if let Err(restore)=clipboard.restore(){eprintln!("{restore:#}");}
            // This test worker can only copy these fixtures. End that exact
            // child on a failed watchdog; production committed operations keep
            // their original lifetime policy.
            if let Some(child)=client.child.as_mut(){let _=child.kill();let _=child.wait();}
            client.complete=true;
            return Err(error.context(stderr));
        }
    };
    let status=read_small::<Status>(&client.workspace.join("status.json"),65536)?;
    ensure!(!external_clipboard_change,"native copy validation: another clipboard change cancelled the test; external clipboard preserved");
    let(copied,before_clipboard_read)=read_clipboard_retry("read native copy result")?;let paths=zsdesk_windows::ole::hdrop_paths(&copied);drop(copied);
    let exact=paths.len()==fixtures.0.len()&&fixtures.0.iter().all(|expected|paths.contains(expected));
    if exact&&before_clipboard_read==unsafe{GetClipboardSequenceNumber()}{clipboard.ours=Some(before_clipboard_read);}
    let evidence=serde_json::json!({"worker_pid":child,"outcome":result,"native_menu_observed_open":status.probe_menu_open,
        "native_owner_command_messages":status.probe_command_messages,"native_menu":status.native_menu,
        "shell_copy_paths_verified":exact,"copied_count":paths.len(),"expected_count":fixtures.0.len(),
        "bind_microseconds":timing.0,"populate_microseconds":timing.1,"worker_stderr":worker_stderr(&client.workspace),"physical_input_generated":false});
    fs::write(root.join("command.current.json"),serde_json::to_vec_pretty(&evidence)?)?;
    ensure!(status.probe_menu_open,"the Windows 11 presenter never reported an open menu: {evidence}");
    ensure!(status.probe_command_messages==1,"native command was not dispatched exactly once on the worker owner: {evidence}");
    ensure!(matches!(result,Ok(MenuOutcome::ShellInvoked|MenuOutcome::NativeHandled)),"native copy did not finish: {evidence}");
    ensure!(exact,"native copy used a different selection: {evidence}");
    for path in &fixtures.0{ensure!(fs::read(path)?==b"ZSDesk native owner command fixture","fixture changed");}
    clipboard.restore()?;drop(clipboard);drop(client);
    let deadline=Instant::now()+Duration::from_secs(5);while pending_workers()>0&&Instant::now()<deadline{pump(10);}
    ensure!(pending_workers()==0,"native menu worker did not retire");
    Ok(evidence)
}

#[test]
#[ignore="Verified Win11 native presenter on a private desktop; temporarily preserves/restores clipboard; requires ZSDESK_NATIVE_TEST_OUTPUT"]
fn native_modern_menu_worker_routes_owner_command_to_exact_selection()->Result<()>{
    let root=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("output")?).join("modern-menu-owner");ensure!(root.is_absolute(),"absolute output required");fs::create_dir_all(&root)?;
    let before=unsafe{GetForegroundWindow()};let name=format!("ZSDesk.ModernMenuOwner.{}",std::process::id());
    let desktop=unsafe{CreateDesktopW(wide(&name).as_ptr(),std::ptr::null(),std::ptr::null(),0,0x000f00ff,std::ptr::null())};ensure!(!desktop.is_null(),"private desktop failed");let raw=desktop as usize;let target=root.clone();
    let result=thread::spawn(move||->Result<serde_json::Value>{ensure!(unsafe{SetThreadDesktop(raw as*mut c_void)}!=0,"SetThreadDesktop failed");exercise_modern_owner(&target,&name)}).join().map_err(|_|anyhow::anyhow!("modern menu owner test panicked")).and_then(|value|value);
    let closed=unsafe{CloseDesktop(desktop)}!=0;let foreground=unsafe{GetForegroundWindow()};let mut pid=0;unsafe{GetWindowThreadProcessId(foreground,Some(&mut pid));}
    let isolated=pid!=std::process::id();let report=serde_json::json!({"state":if result.is_ok()&&closed&&isolated{"passed"}else{"failed"},"checks":result.as_ref().map_err(|error|format!("{error:#}")),"desktop_closed":closed,"foreground_unchanged":before==foreground,"test_did_not_activate":isolated});
    fs::write(root.join("verification.current.json"),serde_json::to_vec_pretty(&report)?)?;println!("{report}");result?;ensure!(closed&&isolated,"desktop cleanup/focus changed");Ok(())
}
