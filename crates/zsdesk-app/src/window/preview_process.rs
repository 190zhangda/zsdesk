//! Disposable preview host. The file-manager STA never calls a document decoder.
use std::{cell::{Cell,RefCell},fs,path::{Path,PathBuf},process::{Child,Command,Stdio},sync::atomic::{AtomicU64,Ordering},time::{Duration,Instant}};
use std::os::windows::{process::CommandExt,io::AsRawHandle,fs::MetadataExt};
use anyhow::{bail,Context,Result};
use serde::{Deserialize,Serialize};
use windows::{core::PCWSTR,Win32::{Foundation::{CloseHandle,HANDLE,HWND,LPARAM,POINT,RECT,WPARAM},
    System::{JobObjects::*,Ole::{OleInitialize,OleUninitialize},Threading::CREATE_NO_WINDOW},UI::WindowsAndMessaging::*}};
use zsdesk_core::model::explorer::PreviewProviderPreference;

pub(super) const TIMER:usize=8;
const LOAD_TIMEOUT:Duration=Duration::from_secs(30);
static SERIAL:AtomicU64=AtomicU64::new(1);
#[derive(Clone,Serialize,Deserialize)]
struct Request{parent:u32,owner:isize,path:PathBuf,provider:PreviewProviderPreference,dark:bool,hover:bool,generation:u64,close:bool,embedded:Option<[i32;4]>}
#[derive(Serialize,Deserialize)]
struct Status{pid:u32,hwnd:isize,generation:u64,ready:bool,kind:String,notice:String}

fn write_json(path:&Path,value:&impl Serialize)->Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Storage::FileSystem::{MoveFileExW,MOVEFILE_REPLACE_EXISTING};
    let temporary=path.with_extension("tmp");fs::write(&temporary,serde_json::to_vec(value)?)?;
    let from:Vec<_>=temporary.as_os_str().encode_wide().chain(Some(0)).collect();
    let to:Vec<_>=path.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe{MoveFileExW(PCWSTR(from.as_ptr()),PCWSTR(to.as_ptr()),MOVEFILE_REPLACE_EXISTING)}?;Ok(())
}
fn cleanup(workspace:&Path){
    // This unique directory contains only this preview's protocol files/PDFs.
    if workspace.file_name().is_none_or(|name|!name.to_string_lossy().starts_with("preview-")){return;}
    if let Ok(items)=fs::read_dir(workspace){for item in items.flatten(){
        let name=item.file_name();let name=name.to_string_lossy();
        if !["request.json","request.tmp","status.json","status.tmp","worker.stderr.log","wps-lifecycle.json"].contains(&name.as_ref()) && !(name.starts_with("wps-")&&name.ends_with(".pdf")){continue;}
        if item.path().symlink_metadata().is_ok_and(|metadata|metadata.is_file()&&metadata.file_attributes()&0x400==0){let _=fs::remove_file(item.path());}
    }}
    let _=fs::remove_dir(workspace);
}

pub(super) struct PreviewProcess {
    child:RefCell<Option<Child>>,job:Option<usize>,workspace:PathBuf,request:Request,
    hwnd:Cell<HWND>,started:Instant,ready:Cell<bool>,reported_exit:bool,
}
impl PreviewProcess {
    pub(super) fn open_with_options(owner:HWND,path:&Path,provider:PreviewProviderPreference,dark:bool,hover:bool)->Result<Self>{
        Self::open_impl(owner,path,provider,dark,hover,None)
    }
    fn open_impl(owner:HWND,path:&Path,provider:PreviewProviderPreference,dark:bool,hover:bool,embedded:Option<[i32;4]>)->Result<Self>{
        let root=std::env::var_os("ZSDESK_PREVIEW_ROOT").map(PathBuf::from)
            .unwrap_or_else(||dirs::cache_dir().unwrap_or_else(std::env::temp_dir).join(crate::edition::profile_name()).join("previews"));
        if !root.is_absolute(){bail!("预览缓存目录必须是绝对路径");}
        fs::create_dir_all(&root)?;
        let workspace=root.join(format!("preview-{}-{}-{}",std::process::id(),SERIAL.fetch_add(1,Ordering::Relaxed),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos()));
        fs::create_dir(&workspace)?;
        let request=Request{parent:std::process::id(),owner:owner.0 as isize,path:path.to_path_buf(),provider,dark,hover,generation:1,close:false,embedded};
        write_json(&workspace.join("request.json"),&request)?;
        let mut command=Command::new(std::env::current_exe()?);
        let stderr=fs::File::create(workspace.join("worker.stderr.log"))?;
        command.arg("--preview-worker").arg(workspace.join("request.json"))
            .env("ZSDESK_PREVIEW_WORKSPACE",&workspace).creation_flags(CREATE_NO_WINDOW.0)
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(stderr);
        let mut child=match command.spawn(){Ok(child)=>child,Err(error)=>{cleanup(&workspace);return Err(error.into());}};
        let job=unsafe{CreateJobObjectW(None,PCWSTR::null())};
        let job=match job{Ok(job)=>job,Err(error)=>{let _=child.kill();let _=child.wait();cleanup(&workspace);return Err(error.into());}};
        let mut limits=JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags=JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let attached=unsafe{SetInformationJobObject(job,JobObjectExtendedLimitInformation,(&limits as*const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),std::mem::size_of_val(&limits)as u32)
            .and_then(|_|AssignProcessToJobObject(job,HANDLE(child.as_raw_handle())))};
        if let Err(error)=attached{let _=child.kill();let _=child.wait();let _=unsafe{CloseHandle(job)};cleanup(&workspace);return Err(error.into());}
        unsafe{SetTimer(Some(owner),TIMER,100,None);}
        Ok(Self{child:RefCell::new(Some(child)),job:Some(job.0 as usize),workspace,request,hwnd:Cell::new(HWND::default()),started:Instant::now(),ready:Cell::new(false),reported_exit:false})
    }
    pub(super) fn is_open(&self)->bool{
        self.child.borrow_mut().as_mut().is_some_and(|child|child.try_wait().ok().flatten().is_none())
    }
    pub(super) fn is_hover(&self)->bool{self.request.hover}
    pub(super) fn contains_point(&self,point:POINT)->bool{
        let mut bounds=RECT::default();let hwnd=self.hwnd.get();
        !hwnd.0.is_null()&&unsafe{GetWindowRect(hwnd,&mut bounds)}.is_ok()&&point.x>=bounds.left&&point.x<bounds.right&&point.y>=bounds.top&&point.y<bounds.bottom
    }
    pub(super) fn owns_window(&self,hwnd:HWND)->bool{hwnd==self.hwnd.get()&&!hwnd.0.is_null()||unsafe{IsChild(self.hwnd.get(),hwnd)}.as_bool()}
    pub(super) fn pre_translate(&mut self,_message:&MSG)->bool{false}
    fn layout(&mut self,bounds:[i32;4]){
        let hwnd=self.hwnd.get();if hwnd.0.is_null(){return;}
        let mut current=RECT::default();let _=unsafe{GetWindowRect(hwnd,&mut current)};
        let mut origin=POINT{x:current.left,y:current.top};let _=unsafe{windows::Win32::Graphics::Gdi::ScreenToClient(HWND(self.request.owner as*mut _),&mut origin)};
        if [origin.x,origin.y,current.right-current.left,current.bottom-current.top]!=bounds{
            let _=unsafe{SetWindowPos(hwnd,None,bounds[0],bounds[1],bounds[2],bounds[3],SWP_NOACTIVATE|SWP_NOZORDER|SWP_ASYNCWINDOWPOS)};
        }
    }
    pub(super) fn set_dark(&mut self,dark:bool)->Result<()>{
        if self.request.dark==dark{return Ok(());}
        let previous=self.request.dark;self.request.dark=dark;
        if let Err(error)=write_json(&self.workspace.join("request.json"),&self.request){self.request.dark=previous;return Err(error);}Ok(())
    }
    pub(super) fn update(&mut self,path:&Path)->Result<()>{
        if self.request.path==path{return Ok(());}
        self.request.path=path.into();self.request.generation=self.request.generation.wrapping_add(1);
        self.started=Instant::now();self.ready.set(false);write_json(&self.workspace.join("request.json"),&self.request)
    }
    pub(super) fn poll(&mut self)->Option<String>{
        let exited=self.child.borrow_mut().as_mut().and_then(|child|child.try_wait().ok().flatten());
        if let Some(exit)=exited {
            if !self.reported_exit && !exit.success(){
                self.reported_exit=true;
                return Some(format!("预览宿主异常退出（{exit}），文件管理窗口可继续使用。"));
            }
            return None;
        }
        if let Ok(bytes)=fs::read(self.workspace.join("status.json")){
            if bytes.len()<65536{if let Ok(status)=serde_json::from_slice::<Status>(&bytes){
                if self.child.borrow().as_ref().is_some_and(|child|child.id()==status.pid){
                    let window=HWND(status.hwnd as*mut _);let mut pid=0;unsafe{GetWindowThreadProcessId(window,Some(&mut pid));}
                    if pid==status.pid {self.hwnd.set(window);}
                    if status.generation==self.request.generation {self.ready.set(status.ready);}
                }
            }}
        }
        if self.is_open()&&!self.ready.get()&&self.started.elapsed()>LOAD_TIMEOUT{
            self.close();return Some("预览加载超时，已关闭预览宿主；文件管理窗口可继续使用。".into());
        }
        None
    }
    pub(super) fn close(&mut self){
        let Some(mut child)=self.child.borrow_mut().take() else{return;};
        self.request.close=true;let _=write_json(&self.workspace.join("request.json"),&self.request);
        let window=self.hwnd.replace(HWND::default());
        if !window.0.is_null(){let _=unsafe{PostMessageW(Some(window),WM_CLOSE,WPARAM(0),LPARAM(0))};}
        let job=self.job.take();let workspace=self.workspace.clone();
        // Only this owned helper is terminated after its grace period; WPS and
        // shared system preview-host processes are never killed by image name.
        std::thread::spawn(move||{
            let deadline=Instant::now()+Duration::from_secs(5);
            while child.try_wait().ok().flatten().is_none()&&Instant::now()<deadline{std::thread::sleep(Duration::from_millis(25));}
            if child.try_wait().ok().flatten().is_none(){let _=child.kill();}
            let _=child.wait();if let Some(job)=job{let _=unsafe{CloseHandle(HANDLE(job as*mut _))};}cleanup(&workspace);
        });
    }
}
impl Drop for PreviewProcess{fn drop(&mut self){self.close();}}

pub(crate) fn run_if_requested()->Option<Result<()>>{
    if std::env::args_os().nth(1).as_deref()!=Some(std::ffi::OsStr::new("--preview-worker")){return None;}
    Some((||{
        let path=PathBuf::from(std::env::args_os().nth(2).context("缺少预览请求")?);
        if std::env::args_os().count()!=3||!path.is_absolute(){bail!("预览请求无效");}
        unsafe{OleInitialize(None)}?;
        let result=run_worker(&path);
        unsafe{OleUninitialize();}result
    })())
}
fn read_request(path:&Path)->Result<Request>{let bytes=fs::read(path)?;if bytes.len()>65536{bail!("预览请求过长");}Ok(serde_json::from_slice(&bytes)?)}
fn owner_alive(request:&Request)->bool{
    let mut pid=0;let hwnd=HWND(request.owner as*mut _);unsafe{GetWindowThreadProcessId(hwnd,Some(&mut pid));}
    pid==request.parent&&unsafe{IsWindow(Some(hwnd))}.as_bool()
}
fn run_worker(path:&Path)->Result<()>{
    let mut request=read_request(path)?;if !owner_alive(&request)||request.close{return Ok(());}
    let owner=HWND(request.owner as*mut _);
    let bounds=request.embedded.map(|r|RECT{left:r[0],top:r[1],right:r[0]+r[2],bottom:r[1]+r[3]});
    let mut preview=super::quick_preview::QuickPreview::create_worker(owner,request.provider,request.dark,request.hover,bounds)?;
    let status_path=path.with_file_name("status.json");
    let load=|preview:&mut super::quick_preview::QuickPreview,request:&Request|->Result<()>{
        let hwnd=preview.window();
        write_json(&status_path,&Status{pid:std::process::id(),hwnd:hwnd.0 as isize,generation:request.generation,ready:false,kind:"loading".into(),notice:String::new()})?;
        preview.update(&request.path)?;
        let (kind,notice)=preview.status();
        write_json(&status_path,&Status{pid:std::process::id(),hwnd:hwnd.0 as isize,generation:request.generation,ready:true,kind,notice})
    };
    load(&mut preview,&request)?;
    unsafe{SetTimer(Some(preview.window()),99,100,None);}
    let mut message=MSG::default();
    while preview.is_open()&&unsafe{GetMessageW(&mut message,None,0,0)}.0>0{
        if request.embedded.is_some() && matches!(message.message,WM_KEYDOWN|WM_SYSKEYDOWN){
            use windows::Win32::UI::Input::KeyboardAndMouse::*;
            let ctrl=unsafe{GetKeyState(VK_CONTROL.0 as i32)}<0;let alt=unsafe{GetKeyState(VK_MENU.0 as i32)}<0;
            let action=match message.wParam.0 as u16{
                key if key==VK_P.0&&alt=>Some(crate::commands::Command::TogglePreview),
                key if key==VK_L.0&&ctrl=>Some(crate::commands::Command::FocusAddress),
                key if key==VK_F11.0=>Some(crate::commands::Command::ToggleFullscreen),
                key if key==VK_F6.0=>Some(if unsafe{GetKeyState(VK_SHIFT.0 as i32)}<0{crate::commands::Command::FocusPrevious}else{crate::commands::Command::FocusNext}),
                _=>None,
            };
            if let Some(action)=action{super::post(owner,super::WM_ACTION,action.id()as usize,0);continue;}
        }
        if message.message==WM_TIMER&&message.wParam.0==99{
            if !owner_alive(&request){break;}
            if let Ok(next)=read_request(path){
                if next.parent!=request.parent||next.owner!=request.owner||next.close{break;}
                if next.dark!=request.dark{preview.set_dark(next.dark);request.dark=next.dark;}
                if next.generation!=request.generation{request=next;load(&mut preview,&request)?;}
            }
            continue;
        }
        if !preview.pre_translate(&message){unsafe{let _=TranslateMessage(&message);DispatchMessageW(&message);}}
    }
    preview.close();Ok(())
}

pub(super) fn sync_pane(owner:HWND,slot:&super::Slot,pane:&mut Option<PreviewProcess>){
    let context=slot.try_borrow().ok().and_then(|slot|slot.as_ref().and_then(|state|state.preview_rect.map(|rect|{
        let path=if state.pending.is_none(){state.browser.as_ref().and_then(|browser|browser.first_selected_path()).unwrap_or_default()}else{PathBuf::new()};
        (path,[rect.x,rect.y,rect.width,rect.height],state.session.ui.preview_provider,state.dark)
    })));
    let Some((path,bounds,provider,dark))=context else{*pane=None;return;};
    if pane.as_ref().is_some_and(|pane|pane.request.provider!=provider||pane.request.dark!=dark||(!pane.is_open()&&pane.request.path!=path)){*pane=None;}
    if pane.is_none(){
        match PreviewProcess::open_impl(owner,&path,provider,dark,false,Some(bounds)){
            Ok(process)=>*pane=Some(process),
            Err(error)=>if let Ok(mut slot)=slot.try_borrow_mut(){if let Some(state)=slot.as_mut(){state.show_message(format!("无法打开预览窗格：{error}"));}},
        }
    }
    if let Some(pane)=pane.as_mut(){let _=pane.update(&path);pane.layout(bounds);}
}
