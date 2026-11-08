//! Large-selection right-click through the real file-manager window delegate.
use super::*;
use anyhow::ensure;
use std::{ffi::c_void,fs,path::Path,thread,time::{Duration,Instant}};
use crate::context_menu_process::isolation_tests::{Environment,sparse};
use zsdesk_windows::shell::menu::QuickAction;

#[link(name="user32")]unsafe extern "system"{
    fn CreateDesktopW(name:*const u16,device:*const u16,mode:*const c_void,flags:u32,access:u32,attributes:*const c_void)->*mut c_void;
    fn SetThreadDesktop(desktop:*mut c_void)->i32;
    fn CloseDesktop(desktop:*mut c_void)->i32;
}
const PROBE:usize=0x5a530920;
thread_local!{static MORE:Cell<bool>=const{Cell::new(false)};static ENTERED:Cell<Option<Instant>>=const{Cell::new(None)};static QUICK:Cell<bool>=const{Cell::new(false)};}
unsafe extern "system" fn probe(hwnd:HWND,msg:u32,wp:WPARAM,lp:LPARAM,_id:usize,_data:usize)->LRESULT{
    if msg==WM_ENTERMENULOOP{ENTERED.with(|value|value.set(Some(Instant::now())));unsafe{SetTimer(Some(hwnd),PROBE,40,None);}}
    if msg==WM_INITMENUPOPUP{
        let menu=HMENU(wp.0 as*mut _);let count=unsafe{GetMenuItemCount(Some(menu))};
        QUICK.with(|value|value.set((0..count).any(|index|unsafe{GetMenuItemID(menu,index)}==QuickAction::More.id())));
    }
    if msg==WM_TIMER&&wp.0==PROBE{
        let _=unsafe{KillTimer(Some(hwnd),PROBE)};
        if MORE.with(Cell::get){let _=unsafe{PostMessageW(Some(hwnd),WM_CHAR,WPARAM(b'm' as usize),LPARAM(1))};}
        else{let _=unsafe{EndMenu()};}return LRESULT(0);
    }
    if msg==WM_NCDESTROY{let _=unsafe{RemoveWindowSubclass(hwnd,Some(probe),PROBE)};}
    unsafe{DefSubclassProc(hwnd,msg,wp,lp)}
}
struct Apartment;impl Drop for Apartment{fn drop(&mut self){unsafe{OleUninitialize();}}}
struct Runner{hwnd:HWND,slot:Box<Slot>}
impl Drop for Runner{fn drop(&mut self){let _=unsafe{DestroyWindow(self.hwnd)};}}
fn pump(ms:u64){let end=Instant::now()+Duration::from_millis(ms);while Instant::now()<end{
    let mut message=MSG::default();for _ in 0..256{if !unsafe{PeekMessageW(&mut message,None,0,0,PM_REMOVE)}.as_bool(){break;}if message.message!=WM_QUIT{unsafe{let _=TranslateMessage(&message);DispatchMessageW(&message);}}}thread::sleep(Duration::from_millis(3));}}
fn create(root:&Path,folder:&Path)->Result<Runner>{
    unsafe{InitCommonControlsEx(&INITCOMMONCONTROLSEX{dwSize:size_of::<INITCOMMONCONTROLSEX>()as u32,dwICC:ICC_STANDARD_CLASSES|ICC_BAR_CLASSES|ICC_LISTVIEW_CLASSES})}.ok()?;
    let mut session=storage::default_session();session.tabs=vec![ExplorerTab::new(1,ShellLocation::Path(folder.into()))];session.active_tab=0;session.next_id=2;
    session.ui.show_preview=false;session.ui.hover_preview=false;session.ui.show_details=false;session.ui.show_tree=false;
    let slot=Box::new(RefCell::new(Some(WindowState::new(session,root.join("profile/explorer-session.json"),None,false))));
    let instance=HINSTANCE(unsafe{GetModuleHandleW(None)?}.0);let class=windows::core::w!("ZSDesk.Native.QuickMenuRegression");
    unsafe{RegisterClassW(&WNDCLASSW{hInstance:instance,lpfnWndProc:Some(window_proc),lpszClassName:class,..Default::default()});}
    let hwnd=unsafe{CreateWindowExW(WS_EX_NOACTIVATE,class,windows::core::w!("Quick context menu regression"),WS_OVERLAPPEDWINDOW|WS_CLIPCHILDREN,100,100,1040,720,None,None,Some(instance),Some((&*slot as*const Slot).cast()))}?;
    let runner=Runner{hwnd,slot};unsafe{runner.slot.borrow_mut().as_mut().unwrap().create(hwnd)?;let _=ShowWindow(hwnd,SW_SHOWNOACTIVATE);}
    let deadline=Instant::now()+Duration::from_secs(12);
    loop{pump(25);if runner.slot.borrow().as_ref().is_some_and(|state|state.pending.is_none()&&state.browser.as_ref().is_some_and(|browser|browser.status().items==3)){break;}
        ensure!(Instant::now()<deadline,"main Shell view did not materialize sparse files");}
    Ok(runner)
}
fn exercise(root:&Path,desktop_name:&str)->Result<serde_json::Value>{
    let _desktop=Environment::set("ZSDESK_MENU_TEST_DESKTOP",desktop_name);let _root=Environment::set("ZSDESK_MENU_ROOT",root.join("workers"));
    unsafe{OleInitialize(None)?;}let _apartment=Apartment;
    let folder=root.join("files");fs::create_dir_all(&folder)?;let(files,_,records)=sparse(&folder)?;
    let runner=create(root,&folder)?;
    let view={let state=runner.slot.borrow();let browser=state.as_ref().unwrap().browser.as_ref().unwrap();browser.select_all(true);browser.view_window().context("view HWND")?};
    pump(80);
    {let state=runner.slot.borrow();let status=state.as_ref().unwrap().browser.as_ref().unwrap().status();ensure!(status.selected==3&&status.selected_bytes==Some(24u64*1024*1024*1024),"large-selection size/count invalid: {status:?}");}
    unsafe{SetWindowSubclass(view,Some(probe),PROBE,0)}.ok()?;
    let mut rows=Vec::new();
    for (entry,position) in [("keyboard",LPARAM(-1)),("mouse",{let mut rect=RECT::default();unsafe{GetWindowRect(view,&mut rect)?;}LPARAM(((rect.left+70)as u32&0xffff|(((rect.top+70)as u32&0xffff)<<16))as isize)})]{
        MORE.with(|value|value.set(false));ENTERED.with(|value|value.set(None));QUICK.with(|value|value.set(false));let before=zsdesk_windows::shell::menu_diagnostics();
        let started=Instant::now();unsafe{SendMessageW(view,WM_CONTEXTMENU,Some(WPARAM(view.0 as usize)),Some(position));}
        let entered=ENTERED.with(Cell::get).context("main context popup did not enter its native menu loop")?;let after=zsdesk_windows::shell::menu_diagnostics();
        ensure!(QUICK.with(Cell::get),"main view bypassed immediate command menu");
        ensure!(before.presenter_creations==after.presenter_creations&&before.last_public_populate_microseconds==after.last_public_populate_microseconds,"immediate menu constructed a Shell extension menu");
        ensure!(super::context_menu::active_pid(runner.hwnd).is_none(),"normal right-click started an extension process");
        rows.push(serde_json::json!({"entry":entry,"input_to_native_menu_loop_microseconds":entered.duration_since(started).as_micros()as u64,"extension_process_started":false,"query_context_menu_called":false}));
    }
    let mut workers=Vec::new();
    for failure in [true,false]{
        let _probe=Environment::set("ZSDESK_MENU_TEST_PROBE",if failure{"crash"}else{"dismiss"});MORE.with(|value|value.set(true));
        unsafe{SendMessageW(view,WM_CONTEXTMENU,Some(WPARAM(view.0 as usize)),Some(LPARAM(-1)));}
        let pid=super::context_menu::active_pid(runner.hwnd).context("More action did not start extension process")?;ensure!(pid!=std::process::id(),"extension process was not isolated");
        let deadline=Instant::now()+Duration::from_secs(10);while super::context_menu::active_pid(runner.hwnd).is_some(){pump(20);ensure!(Instant::now()<deadline,"main extension result did not settle");}
        ensure!(unsafe{IsWindow(Some(runner.hwnd))}.as_bool()&&unsafe{IsWindow(Some(view))}.as_bool(),"extension failure closed main window");
        if failure{ensure!(runner.slot.borrow().as_ref().unwrap().status.contains("右键菜单加载失败"),"extension failure did not produce user feedback");}
        workers.push(serde_json::json!({"injected_failure":failure,"child_pid":pid,"parent_survived":true,"context_menu_retry_available":true}));
    }
    let _=unsafe{RemoveWindowSubclass(view,Some(probe),PROBE)};drop(runner);pump(30);drop(files);
    let deadline=Instant::now()+Duration::from_secs(5);while crate::context_menu_process::pending_workers()>0&&Instant::now()<deadline{pump(10);}ensure!(crate::context_menu_process::pending_workers()==0,"context worker remains running");
    Ok(serde_json::json!({"selection_bytes":24u64*1024*1024*1024,"files":records,"immediate_menu":rows,"workers":workers,"main_delegate_installed":true,"fixtures_removed":true}))
}
#[test]
#[ignore="Real main window and 24 GiB sparse selection on private desktop; requires ZSDESK_NATIVE_TEST_OUTPUT"]
fn native_main_window_large_selection_immediate_context_menu()->Result<()>{
    let root=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("output")?).join("main-quick-menu");ensure!(root.is_absolute(),"absolute output required");fs::create_dir_all(&root)?;
    let before=unsafe{GetForegroundWindow()};let name=format!("ZSDesk.MainQuickMenu.{}",std::process::id());let desktop=unsafe{CreateDesktopW(wide(&name).as_ptr(),std::ptr::null(),std::ptr::null(),0,0x000f00ff,std::ptr::null())};ensure!(!desktop.is_null(),"CreateDesktop failed");let raw=desktop as usize;let destination=root.clone();
    let result=thread::spawn(move||->Result<serde_json::Value>{ensure!(unsafe{SetThreadDesktop(raw as*mut c_void)}!=0,"SetThreadDesktop failed");exercise(&destination,&name)}).join().map_err(|_|anyhow::anyhow!("main quick menu test panicked")).and_then(|value|value);
    let closed=unsafe{CloseDesktop(desktop)}!=0;let current=unsafe{GetForegroundWindow()};let mut process=0;unsafe{GetWindowThreadProcessId(current,Some(&mut process));}
    let isolated=process!=std::process::id();let report=serde_json::json!({"state":if result.is_ok()&&closed&&isolated{"passed"}else{"failed"},"checks":result.as_ref().map_err(|error|format!("{error:#}")),"desktop_closed":closed,"foreground_unchanged":before==current,"test_did_not_activate":isolated});
    fs::write(root.join("verification.current.json"),serde_json::to_vec_pretty(&report)?)?;println!("{report}");result?;ensure!(closed&&isolated,"desktop cleanup/focus failed");Ok(())
}
