//! Both native menu styles isolate extensions for selection and background requests.
use super::*;
use anyhow::ensure;
use std::{ffi::c_void,fs,path::Path,thread,time::{Duration,Instant}};
use crate::context_menu_process::isolation_tests::{Environment,sparse};

#[link(name="user32")]unsafe extern "system"{
    fn CreateDesktopW(name:*const u16,device:*const u16,mode:*const c_void,flags:u32,access:u32,attributes:*const c_void)->*mut c_void;
    fn SetThreadDesktop(desktop:*mut c_void)->i32;
    fn CloseDesktop(desktop:*mut c_void)->i32;
}
struct Apartment;impl Drop for Apartment{fn drop(&mut self){unsafe{OleUninitialize();}}}
struct Runner{hwnd:HWND,slot:Box<Slot>}
impl Drop for Runner{fn drop(&mut self){let _=unsafe{DestroyWindow(self.hwnd)};}}
fn pump(ms:u64){let end=Instant::now()+Duration::from_millis(ms);while Instant::now()<end{
    let mut message=MSG::default();for _ in 0..256{if !unsafe{PeekMessageW(&mut message,None,0,0,PM_REMOVE)}.as_bool(){break;}if message.message!=WM_QUIT{unsafe{let _=TranslateMessage(&message);DispatchMessageW(&message);}}}thread::sleep(Duration::from_millis(3));}}
fn create(root:&Path,folder:&Path,modern:bool)->Result<Runner>{
    unsafe{InitCommonControlsEx(&INITCOMMONCONTROLSEX{dwSize:size_of::<INITCOMMONCONTROLSEX>()as u32,dwICC:ICC_STANDARD_CLASSES|ICC_BAR_CLASSES|ICC_LISTVIEW_CLASSES})}.ok()?;
    let mut session=storage::default_session();session.tabs=vec![ExplorerTab::new(1,ShellLocation::Path(folder.into()))];session.active_tab=0;session.next_id=2;
    session.ui.show_preview=false;session.ui.hover_preview=false;session.ui.show_details=false;session.ui.show_tree=false;
    let slot=Box::new(RefCell::new(Some(WindowState::new(session,root.join("profile/explorer-session.json"),None,modern))));
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
    struct Workers;
    impl Drop for Workers{fn drop(&mut self){
        crate::context_menu_process::shutdown_idle_worker();let deadline=Instant::now()+Duration::from_secs(5);
        while crate::context_menu_process::pending_workers()>0&&Instant::now()<deadline{pump(10);}
    }}
    let _workers=Workers;
    let _desktop=Environment::set("ZSDESK_MENU_TEST_DESKTOP",desktop_name);
    let _root=Environment::set("ZSDESK_MENU_ROOT",root.join("workers"));
    unsafe{OleInitialize(None)?;}let _apartment=Apartment;
    let folder=root.join("files");fs::create_dir_all(&folder)?;let(files,_,records)=sparse(&folder)?;
    let mut rows=Vec::new();
    for modern in [false,true]{
        let runner=create(&root.join(if modern{"modern"}else{"classic"}),&folder,modern)?;
        for selected in [false,true]{
            let view={let state=runner.slot.borrow();let browser=state.as_ref().unwrap().browser.as_ref().unwrap();
                browser.select_all(selected);browser.view_window().context("view HWND")?};
            pump(60);
            {let state=runner.slot.borrow();let status=state.as_ref().unwrap().browser.as_ref().unwrap().status();
                ensure!(status.selected==if selected{3}else{0},"selection/background setup failed");
                if selected{ensure!(status.selected_bytes==Some(24u64*1024*1024*1024),"large-selection size truncated");}}
            for crash in [true,false]{
                let _probe=Environment::set("ZSDESK_MENU_TEST_PROBE",if crash{"crash"}else{"dismiss"});
                let before=zsdesk_windows::shell::menu_diagnostics();
                let mut bounds=RECT::default();unsafe{GetWindowRect(view,&mut bounds)?;}
                let position=if selected{LPARAM(-1)}else{LPARAM((((bounds.left+60)as u16 as u32)|(((bounds.bottom-40)as u16 as u32)<<16))as isize)};
                let started=Instant::now();
                ensure!(unsafe{SendMessageW(view,WM_CONTEXTMENU,Some(WPARAM(view.0 as usize)),Some(position))}.0==0,"view did not consume menu request");
                let dispatch_microseconds=started.elapsed().as_micros()as u64;
                ensure!(super::context_menu::request_active(runner.hwnd),"main view bypassed async menu worker");
                let mut pid=None;
                let deadline=Instant::now()+Duration::from_secs(15);
                while super::context_menu::request_active(runner.hwnd){
                    if let Some(value)=super::context_menu::active_pid(runner.hwnd){pid=Some(value);ensure!(value!=std::process::id(),"extension ran in main process");}
                    pump(10);ensure!(Instant::now()<deadline,"isolated menu result did not settle");
                }
                let recorded=crate::context_menu_process::read_recent_diagnostics();
                let pid=pid.or_else(||recorded["current"]["worker_pid"].as_u64().map(|value|value as u32)).context("async worker PID not recorded")?;
                ensure!(pid!=std::process::id(),"extension ran in main process");
                let after=zsdesk_windows::shell::menu_diagnostics();
                ensure!(before.presenter_creations==after.presenter_creations&&before.last_public_populate_microseconds==after.last_public_populate_microseconds,
                    "modern/classic menu constructed extension state in the main process");
                ensure!(unsafe{IsWindow(Some(runner.hwnd))}.as_bool()&&unsafe{IsWindow(Some(view))}.as_bool(),"extension crash closed main view");
                if crash{ensure!(runner.slot.borrow().as_ref().unwrap().status.contains("右键菜单加载失败"),"crash did not report a retryable failure");}
                rows.push(serde_json::json!({"modern_preference":modern,"target":if selected{"selection"}else{"background"},
                    "injected_crash":crash,"child_pid":pid,"parent_survived":true,"dispatch_microseconds":dispatch_microseconds,
                    "main_presenter_or_extension_created":false,"worker_rendering_exercised":false}));
            }
        }
        // A virtual root has no filesystem path; render its actual background
        // menu repeatedly, including after a previous worker has been reused.
        unsafe{runner.slot.borrow_mut().as_mut().unwrap().navigate_to(ShellLocation::ThisPc);}
        let deadline=Instant::now()+Duration::from_secs(12);
        loop {pump(20);let slot=runner.slot.borrow();let state=slot.as_ref().unwrap();
            if state.pending.is_none()&&state.current_location()==&ShellLocation::ThisPc{break;}
            ensure!(Instant::now()<deadline,"This PC navigation did not settle");
        }
        for cycle in 0..5 {
            let command=cycle==4;
            let _probe=Environment::set("ZSDESK_MENU_TEST_PROBE",if command{"background-details"}else{"menu-dismiss"});
            if command{runner.slot.borrow().as_ref().unwrap().browser.as_ref().unwrap().set_view_mode(1,48)?;pump(20);}
            let view={let slot=runner.slot.borrow();let browser=slot.as_ref().unwrap().browser.as_ref().unwrap();browser.select_all(false);browser.view_window().context("This PC view")?};
            let mut bounds=RECT::default();unsafe{GetWindowRect(view,&mut bounds)?;}
            let position=LPARAM((((bounds.left+80)as u16 as u32)|(((bounds.bottom-60)as u16 as u32)<<16))as isize);
            unsafe{SendMessageW(view,WM_CONTEXTMENU,Some(WPARAM(view.0 as usize)),Some(position));}
            ensure!(super::context_menu::request_active(runner.hwnd),"This PC background did not reach its worker");
            let deadline=Instant::now()+Duration::from_secs(15);
            while super::context_menu::request_active(runner.hwnd){pump(10);ensure!(Instant::now()<deadline,"This PC menu did not dismiss");}
            let diagnostic=crate::context_menu_process::read_recent_diagnostics();let current=&diagnostic["current"];
            ensure!(current["error"].is_null(),"This PC background menu failed (modern={modern}, cycle={cycle}): {current}");
            if command{ensure!(runner.slot.borrow().as_ref().unwrap().browser.as_ref().unwrap().view_mode().is_some_and(|(mode,_)|mode==4),"background view command did not reach the main view: {current}");}
            else{ensure!(current["timing"]["menu_open_observation"].is_string(),"This PC background never displayed a menu (modern={modern}, cycle={cycle}): {current}");}
            rows.push(serde_json::json!({"modern_preference":modern,"target":"this-pc-background","cycle":cycle,"diagnostic":current}));
        }
        drop(runner);pump(30);
        crate::context_menu_process::shutdown_idle_worker();
        let deadline=Instant::now()+Duration::from_secs(5);
        while crate::context_menu_process::pending_workers()>0&&Instant::now()<deadline{pump(10);}
        ensure!(crate::context_menu_process::pending_workers()==0,"context worker remains running");
    }
    drop(files);
    Ok(serde_json::json!({"selection_bytes":24u64*1024*1024*1024,"files":records,"routes":rows,
        "main_delegate_installed":true,"fixtures_removed":true,"rendering_scope":"selection fault containment; actual classic/modern This PC background presentation and view command roundtrip"}))
}

#[test]
#[ignore="Main view classic/modern selection/background isolation; requires ZSDESK_NATIVE_TEST_OUTPUT"]
fn native_main_window_background_and_selection_menu_isolation()->Result<()>{
    let root=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("output")?).join("main-context-menu");ensure!(root.is_absolute(),"absolute output required");fs::create_dir_all(&root)?;
    let before=unsafe{GetForegroundWindow()};let name=format!("ZSDesk.MainContextMenu.{}",std::process::id());let desktop=unsafe{CreateDesktopW(wide(&name).as_ptr(),std::ptr::null(),std::ptr::null(),0,0x000f00ff,std::ptr::null())};ensure!(!desktop.is_null(),"CreateDesktop failed");let raw=desktop as usize;let destination=root.clone();
    let result=thread::spawn(move||->Result<serde_json::Value>{ensure!(unsafe{SetThreadDesktop(raw as*mut c_void)}!=0,"SetThreadDesktop failed");exercise(&destination,&name)}).join().map_err(|_|anyhow::anyhow!("main quick menu test panicked")).and_then(|value|value);
    // Shell releases auxiliary STA windows after the owning UI thread exits.
    let deadline=Instant::now()+Duration::from_secs(3);
    let closed=loop{if unsafe{CloseDesktop(desktop)}!=0{break true;}if Instant::now()>=deadline{break false;}thread::sleep(Duration::from_millis(20));};
    let current=unsafe{GetForegroundWindow()};let mut process=0;unsafe{GetWindowThreadProcessId(current,Some(&mut process));}
    let isolated=process!=std::process::id();let report=serde_json::json!({"state":if result.is_ok()&&closed&&isolated{"passed"}else{"failed"},"checks":result.as_ref().map_err(|error|format!("{error:#}")),"desktop_closed":closed,"foreground_unchanged":before==current,"test_did_not_activate":isolated});
    fs::write(root.join("verification.current.json"),serde_json::to_vec_pretty(&report)?)?;println!("{report}");result?;ensure!(closed&&isolated,"desktop cleanup/focus failed");Ok(())
}
