//! On-demand isolation for Shell extensions. The main process never constructs
//! their menus; one request owns one temporary worker and an immutable snapshot.
use std::{fs,io::Read,path::{Path,PathBuf},process::{Child,Command,Stdio},sync::{Mutex,atomic::{AtomicU64,Ordering}},time::{Duration,Instant},thread};
use std::os::windows::{process::CommandExt,fs::MetadataExt,io::AsRawHandle};
use anyhow::{Context,Result,ensure};
use serde::{Deserialize,Serialize};
use windows::{core::{PCWSTR,w},Win32::{Foundation::{HWND,HINSTANCE,LPARAM,LRESULT,WPARAM,POINT,HANDLE,CloseHandle,FILETIME},
    System::{LibraryLoader::GetModuleHandleW,Ole::{OleInitialize,OleUninitialize},Threading::{OpenProcess,GetProcessTimes,PROCESS_QUERY_LIMITED_INFORMATION},DataExchange::GetClipboardOwner,JobObjects::*},
    UI::WindowsAndMessaging::*}};
use zsdesk_windows::shell::menu::{ContextMenuOptions,MenuInterceptions,MenuOutcome,ShellMenu,forward_menu_message};

const MAX_REQUEST:u64=16*1024*1024;
const MAX_ITEMS:usize=8192;
const WORKER_MEMORY_LIMIT:usize=512*1024*1024;
static SERIAL:AtomicU64=AtomicU64::new(1);
static REAPERS:Mutex<Vec<thread::JoinHandle<()>>>=Mutex::new(Vec::new());
pub fn pending_workers()->usize{let mut workers=REAPERS.lock().unwrap_or_else(|error|error.into_inner());let mut index=0;while index<workers.len(){if workers[index].is_finished(){let _=workers.swap_remove(index).join();}else{index+=1;}}workers.len()}
#[link(name="kernel32")]unsafe extern "system"{
    fn SetErrorMode(mode:u32)->u32;
    #[cfg(test)]fn RaiseException(code:u32,flags:u32,count:u32,arguments:*const usize);
}
#[derive(Clone,Copy,Debug,Serialize,Deserialize,PartialEq,Eq)]
pub enum Mode{More,Properties,Open,Background}
#[derive(Clone,Serialize,Deserialize)]
struct Request{
    token:String,parent:u32,parent_created:u64,owner:isize,point:[i32;2],pidls:Vec<Vec<u8>>,folder:Option<PathBuf>,
    options:ContextMenuOptions,rename:bool,open:bool,mode:Mode,extended:bool,
    #[cfg(test)]probe:Option<String>,
}
#[derive(Serialize,Deserialize)]
struct Status{token:String,pid:u32,window:isize,phase:String,validation:u64,before_invoke:bool,outcome:Option<MenuOutcome>,error:Option<String>,bind_microseconds:u64,populate_microseconds:u64,
    #[serde(default)]native_menu:serde_json::Value,
    #[cfg(test)]#[serde(default)]probe_menu_open:bool,
    #[cfg(test)]#[serde(default)]probe_command_messages:u64,
}
#[derive(Serialize,Deserialize)]
struct Decision{token:String,validation:u64,permit:bool}
fn wide(value:&str)->Vec<u16>{value.encode_utf16().chain(Some(0)).collect()}
fn created(pid:u32)->Option<u64>{
    let process=unsafe{OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION,false,pid)}.ok()?;
    let(mut begin,mut end,mut kernel,mut user)=(FILETIME::default(),FILETIME::default(),FILETIME::default(),FILETIME::default());
    let result=unsafe{GetProcessTimes(process,&mut begin,&mut end,&mut kernel,&mut user)};let _=unsafe{CloseHandle(process)};
    result.ok().map(|_|u64::from(begin.dwHighDateTime)<<32|u64::from(begin.dwLowDateTime))
}
fn parent_alive(request:&Request)->bool{
    let mut pid=0;let hwnd=HWND(request.owner as*mut _);unsafe{GetWindowThreadProcessId(hwnd,Some(&mut pid));}
    pid==request.parent&&created(pid)==Some(request.parent_created)&&unsafe{IsWindow(Some(hwnd))}.as_bool()
}
fn write(path:&Path,value:&impl Serialize)->Result<()>{crate::quick_search::control::atomic_write(path,&serde_json::to_vec(value)?)?;Ok(())}
fn read_small<T:serde::de::DeserializeOwned>(path:&Path,limit:u64)->Result<T>{
    let metadata=fs::symlink_metadata(path)?;ensure!(metadata.is_file()&&metadata.file_attributes()&0x400==0&&metadata.len()<=limit,"菜单协议文件无效");
    let mut bytes=Vec::new();fs::File::open(path)?.take(limit+1).read_to_end(&mut bytes)?;ensure!(bytes.len()as u64<=limit,"菜单协议文件过大");Ok(serde_json::from_slice(&bytes)?)
}
fn worker_stderr(workspace:&Path)->String{
    let path=workspace.join("worker.stderr.log");
    let Ok(metadata)=fs::symlink_metadata(&path)else{return String::new();};
    if !metadata.is_file()||metadata.file_attributes()&0x400!=0{return String::new();}
    let mut bytes=Vec::new();
    if fs::File::open(path).and_then(|file|file.take(4096).read_to_end(&mut bytes)).is_err(){return String::new();}
    String::from_utf8_lossy(&bytes).trim().to_owned()
}
fn valid_pidl(bytes:&[u8])->bool{
    if bytes.len()<2||bytes.len()>65536{return false;}
    let mut offset=0usize;
    while offset+2<=bytes.len(){let size=u16::from_le_bytes([bytes[offset],bytes[offset+1]])as usize;
        if size==0{return offset+2==bytes.len();}if size<2{return false;}let Some(next)=offset.checked_add(size)else{return false;};if next>bytes.len(){return false;}offset=next;
    }false
}
fn validate(request:&Request)->Result<()>{
    ensure!(!request.token.is_empty()&&request.token.len()<128,"菜单请求标识无效");
    ensure!(request.pidls.len()<=MAX_ITEMS&&request.pidls.iter().all(|pidl|valid_pidl(pidl)),"菜单选择数据无效或过多");
    ensure!(request.pidls.iter().map(Vec::len).sum::<usize>()<=3*1024*1024,"菜单选择标识总量过大");
    ensure!(!request.pidls.is_empty()||request.folder.is_some(),"没有可用的系统菜单目标");
    ensure!(request.mode!=Mode::Background||request.pidls.len()==1,"背景菜单需要一个文件夹标识");
    ensure!(request.folder.as_ref().is_none_or(|folder|folder.is_absolute()),"菜单目录必须是绝对路径");
    ensure!(request.options.hidden_verbs.len()<=128&&request.options.hidden_verbs.iter().all(|verb|verb.len()<=128&&!verb.contains('\0')),"菜单设置无效");
    Ok(())
}
fn cleanup(workspace:&Path){
    if workspace.file_name().is_none_or(|name|!name.to_string_lossy().starts_with("menu-")){return;}
    for name in ["request.json","status.json","decision.json","cancel","start","worker.stderr.log"]{
        let path=workspace.join(name);if fs::symlink_metadata(&path).is_ok_and(|metadata|metadata.is_file()&&metadata.file_attributes()&0x400==0){let _=fs::remove_file(path);}
    }let _=fs::remove_dir(workspace);
}

pub struct Client{child:Option<Child>,job:Option<usize>,workspace:PathBuf,token:String,started:Instant,last_validation:u64,committed:bool,complete:bool}
impl Client{
    pub fn spawn(owner:HWND,point:POINT,pidls:Vec<Vec<u8>>,folder:Option<PathBuf>,options:ContextMenuOptions,intercept:MenuInterceptions,mode:Mode)->Result<Self>{
        Self::spawn_extended(owner,point,pidls,folder,options,intercept,mode,false)
    }
    pub fn spawn_extended(owner:HWND,point:POINT,pidls:Vec<Vec<u8>>,folder:Option<PathBuf>,options:ContextMenuOptions,intercept:MenuInterceptions,mode:Mode,extended:bool)->Result<Self>{
        let root=std::env::var_os("ZSDESK_MENU_ROOT").map(PathBuf::from).unwrap_or_else(||dirs::cache_dir().unwrap_or_else(std::env::temp_dir).join(crate::edition::profile_name()).join("menus"));
        ensure!(root.is_absolute(),"菜单缓存目录必须是绝对路径");fs::create_dir_all(&root)?;
        let token=format!("{}-{}-{}",std::process::id(),SERIAL.fetch_add(1,Ordering::Relaxed),std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos());
        let workspace=root.join(format!("menu-{token}"));fs::create_dir(&workspace)?;
        let request=Request{token:token.clone(),parent:std::process::id(),parent_created:created(std::process::id()).context("无法核对菜单父进程")?,owner:owner.0 as isize,
            point:[point.x,point.y],pidls,folder,options,rename:intercept.rename,open:intercept.open,mode,extended,
            #[cfg(test)]probe:std::env::var("ZSDESK_MENU_TEST_PROBE").ok(),};
        if let Err(error)=validate(&request).and_then(|_|write(&workspace.join("request.json"),&request)){cleanup(&workspace);return Err(error);}
        let stderr=fs::File::create(workspace.join("worker.stderr.log"))?;let mut command=Command::new(std::env::current_exe()?);
        #[cfg(not(test))]command.arg("--context-menu-worker").arg(workspace.join("request.json"));
        #[cfg(test)]command.arg("context_menu_process::tests::native_extension_worker_entry").args(["--ignored","--exact","--nocapture"]);
        command.env("ZSDESK_MENU_WORKSPACE",&workspace).env("ZSDESK_MENU_TOKEN",&token)
            .creation_flags(0x08000000).stdin(Stdio::null()).stdout(Stdio::null()).stderr(stderr);
        let mut child=match command.spawn(){Ok(child)=>child,Err(error)=>{cleanup(&workspace);return Err(error.into());}};
        // The worker waits for "start" before COM activation, closing the race
        // between CreateProcess and assigning its resource boundary. Children
        // started by a chosen Open command silently leave this job, so an
        // editor launched by the user is neither limited nor killed with it.
        let job=match unsafe{CreateJobObjectW(None,PCWSTR::null())}{Ok(job)=>job,Err(error)=>{let _=child.kill();let _=child.wait();cleanup(&workspace);return Err(error.into());}};
        let mut limits=JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags=JOB_OBJECT_LIMIT_PROCESS_MEMORY|JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK|JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
        limits.ProcessMemoryLimit=WORKER_MEMORY_LIMIT;
        if let Err(error)=unsafe{SetInformationJobObject(job,JobObjectExtendedLimitInformation,(&limits as*const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),std::mem::size_of_val(&limits)as u32)
            .and_then(|_|AssignProcessToJobObject(job,HANDLE(child.as_raw_handle())))}{
            let _=child.kill();let _=child.wait();let _=unsafe{CloseHandle(job)};cleanup(&workspace);return Err(error.into());
        }
        if let Err(error)=write(&workspace.join("start"),&token){let _=child.kill();let _=child.wait();let _=unsafe{CloseHandle(job)};cleanup(&workspace);return Err(error);}
        let _=unsafe{AllowSetForegroundWindow(child.id())};
        Ok(Self{child:Some(child),job:Some(job.0 as usize),workspace,token,started:Instant::now(),last_validation:0,committed:false,complete:false})
    }
    pub fn pid(&self)->Option<u32>{self.child.as_ref().map(Child::id)}
    pub fn timings(&self)->Option<(u64,u64)>{read_small::<Status>(&self.workspace.join("status.json"),65536).ok().filter(|status|status.token==self.token&&self.pid()==Some(status.pid)).map(|status|(status.bind_microseconds,status.populate_microseconds))}
    pub fn poll(&mut self,before:&mut dyn FnMut(bool)->bool)->Option<Result<MenuOutcome>>{
        if self.complete{return None;}
        let status=read_small::<Status>(&self.workspace.join("status.json"),65536).ok().filter(|status|status.token==self.token&&self.pid()==Some(status.pid));
        if let Some(status)=&status{
            if status.phase=="validate"&&status.validation>self.last_validation{
                let permit=before(status.before_invoke);self.last_validation=status.validation;
                // Publish commitment before granting execution. Drop may only
                // terminate workers that have never received this grant.
                if permit&&status.before_invoke{self.committed=true;}
                if let Err(error)=write(&self.workspace.join("decision.json"),&Decision{token:self.token.clone(),validation:status.validation,permit}){self.complete=true;return Some(Err(error));}
            }
            if status.phase=="done"{
                self.complete=true;
                return Some(match &status.error{Some(error)=>Err(anyhow::anyhow!(error.clone())),None=>Ok(status.outcome.unwrap_or(MenuOutcome::Dismissed))});
            }
        }
        if let Some(exit)=self.child.as_mut().and_then(|child|child.try_wait().ok().flatten()){
            self.complete=true;
            let final_state=read_small::<Status>(&self.workspace.join("status.json"),65536).ok().filter(|status|status.token==self.token&&self.pid()==Some(status.pid)&&status.phase=="done");
            return Some(if !exit.success(){
                let detail=worker_stderr(&self.workspace);
                Err(anyhow::anyhow!("系统扩展菜单异常退出（{exit}）；文件管理窗口可继续使用，请重新右键{}",if detail.is_empty(){String::new()}else{format!("\n{detail}")}))
            }
                else if let Some(status)=final_state{match status.error{Some(error)=>Err(anyhow::anyhow!(error)),None=>Ok(status.outcome.unwrap_or(MenuOutcome::Dismissed))}}
                else{Err(anyhow::anyhow!("系统扩展菜单未返回完整结果，请重试"))});
        }
        if !self.committed&&self.started.elapsed()>Duration::from_secs(30)&&status.as_ref().is_none_or(|status|status.phase=="starting"||status.phase=="validate"){
            self.complete=true;self.cancel();return Some(Err(anyhow::anyhow!("系统扩展菜单加载超时；可继续使用常用文件命令")));
        }
        None
    }
    pub fn cancel(&mut self){let _=fs::write(self.workspace.join("cancel"),b"cancel");if !self.committed{if let Some(child)=self.child.as_mut(){let _=child.kill();let _=child.wait();}}}
}
impl Drop for Client{fn drop(&mut self){
    if !self.complete{self.cancel();}
    if let Some(mut child)=self.child.take(){
        let job=self.job.take();
        if child.try_wait().ok().flatten().is_some(){if let Some(job)=job{let _=unsafe{CloseHandle(HANDLE(job as*mut _))};}cleanup(&self.workspace);}
        else{let workspace=self.workspace.clone();if let Ok(worker)=thread::Builder::new().name("zsdesk-menu-reaper".into()).spawn(move||{let _=child.wait();if let Some(job)=job{let _=unsafe{CloseHandle(HANDLE(job as*mut _))};}cleanup(&workspace);}){REAPERS.lock().unwrap_or_else(|error|error.into_inner()).push(worker);}}
    }
}}

struct Worker{request:Request,path:PathBuf,status:Status}
struct Watch{request:Request,path:PathBuf,
    #[cfg(test)]command:std::cell::Cell<Option<u32>>,
    #[cfg(test)]menu_open:std::cell::Cell<bool>,
}
#[cfg(test)]extern "C"{
    fn zs_menu_is_open(owner:HWND)->i32;
    fn zs_menu_command_messages()->u64;
}
impl Worker{
    fn state(&mut self,phase:&str)->Result<()>{self.status.phase=phase.into();self.status.native_menu=serde_json::to_value(zsdesk_windows::shell::menu_diagnostics())?;write(&self.path.with_file_name("status.json"),&self.status)}
    fn allow(&mut self,before_invoke:bool)->windows::core::Result<()>{
        let result=(||->Result<()>{
            ensure!(parent_alive(&self.request)&&!self.path.with_file_name("cancel").exists(),"菜单已取消");
            self.status.validation+=1;self.status.before_invoke=before_invoke;self.state("validate")?;
            let deadline=Instant::now()+Duration::from_secs(3);
            loop{
                if let Ok(answer)=read_small::<Decision>(&self.path.with_file_name("decision.json"),65536){
                    if answer.token==self.request.token&&answer.validation==self.status.validation{ensure!(answer.permit,"菜单选择已改变");break;}
                }
                ensure!(parent_alive(&self.request)&&!self.path.with_file_name("cancel").exists()&&Instant::now()<deadline,"菜单已取消");
                pump(10);
            }
            self.state(if before_invoke{"invoking"}else{"tracking"})?;Ok(())
        })();result.map_err(|error|windows::core::Error::new(windows::core::HRESULT(0x80004004u32 as i32),error.to_string()))
    }
}
fn pump(ms:u64){let end=Instant::now()+Duration::from_millis(ms);while Instant::now()<end{
    let mut message=MSG::default();for _ in 0..64{if !unsafe{PeekMessageW(&mut message,None,0,0,PM_REMOVE)}.as_bool(){break;}if message.message!=WM_QUIT{unsafe{let _=TranslateMessage(&message);DispatchMessageW(&message);}}}thread::sleep(Duration::from_millis(2));}}
unsafe extern "system" fn procedure(hwnd:HWND,msg:u32,wp:WPARAM,lp:LPARAM)->LRESULT{
    if msg==WM_NCCREATE{let create=unsafe{&*(lp.0 as*const CREATESTRUCTW)};unsafe{SetWindowLongPtrW(hwnd,GWLP_USERDATA,create.lpCreateParams as isize);}return LRESULT(1);}
    #[cfg(test)]if msg==WM_ENTERMENULOOP{
        let state=unsafe{GetWindowLongPtrW(hwnd,GWLP_USERDATA)}as*const Watch;
        if !state.is_null()&&unsafe{&*state}.request.probe.as_deref()==Some("menu-dismiss"){unsafe{SetTimer(Some(hwnd),2,40,None);}}
    }
    #[cfg(test)]if msg==WM_TIMER&&wp.0==2{let _=unsafe{KillTimer(Some(hwnd),2)};let _=unsafe{EndMenu()};return LRESULT(0);}
    if msg==WM_TIMER{
        let state=unsafe{GetWindowLongPtrW(hwnd,GWLP_USERDATA)}as*const Watch;
        if !state.is_null(){let state=unsafe{&*state};if !parent_alive(&state.request)||state.path.with_file_name("cancel").exists(){let _=unsafe{EndMenu()};}
            #[cfg(test)]if state.request.probe.as_deref()==Some("modern-owner-copy")&&unsafe{zs_menu_is_open(hwnd)}!=0{
                state.menu_open.set(true);
                if let Some(command)=state.command.take(){
                    let message=unsafe{RegisterWindowMessageW(w!("FILE_EXPLORER_CONTEXTMENU_INVOKEMENUITEM"))};
                    if message!=0{let _=unsafe{PostMessageW(Some(hwnd),message,WPARAM(command as usize),LPARAM(0))};}
                }
            }
        }
    }
    if let Some(result)=forward_menu_message(msg,wp,lp){return result;}
    unsafe{DefWindowProcW(hwnd,msg,wp,lp)}
}
fn run_worker(path:&Path)->Result<()>{
    ensure!(path.is_absolute()&&path.file_name().is_some_and(|name|name=="request.json"),"菜单请求路径无效");
    ensure!(std::env::var_os("ZSDESK_MENU_WORKSPACE").map(PathBuf::from).as_deref()==path.parent(),"菜单工作目录不匹配");
    let request:Request=read_small(path,MAX_REQUEST)?;validate(&request)?;
    ensure!(std::env::var("ZSDESK_MENU_TOKEN").ok().as_deref()==Some(request.token.as_str())&&parent_alive(&request),"菜单请求归属无效");
    let deadline=Instant::now()+Duration::from_secs(5);
    loop{if read_small::<String>(&path.with_file_name("start"),256).is_ok_and(|token|token==request.token){break;}
        ensure!(parent_alive(&request)&&Instant::now()<deadline,"菜单进程尚未建立资源边界");thread::sleep(Duration::from_millis(10));}
    unsafe{SetErrorMode(0x0001|0x0002);OleInitialize(None)?;}
    struct Apartment;impl Drop for Apartment{fn drop(&mut self){unsafe{OleUninitialize();}}}let _apartment=Apartment;
    let mut state=Box::new(Worker{status:Status{token:request.token.clone(),pid:std::process::id(),window:0,phase:"starting".into(),validation:0,before_invoke:false,outcome:None,error:None,bind_microseconds:0,populate_microseconds:0,native_menu:serde_json::Value::Null,
        #[cfg(test)]probe_menu_open:false,#[cfg(test)]probe_command_messages:0},request,path:path.into()});
    state.state("starting")?;
    #[cfg(test)]if state.request.probe.as_deref()==Some("crash"){
        unsafe{RaiseException(0xc0000005,1,0,std::ptr::null());}std::process::exit(0x51);
    }
    #[cfg(test)]if state.request.probe.as_deref()==Some("memory-limit"){
        use windows::Win32::System::Memory::{VirtualAlloc,VirtualFree,MEM_COMMIT,MEM_RESERVE,MEM_RELEASE,PAGE_READWRITE};
        let memory=unsafe{VirtualAlloc(None,WORKER_MEMORY_LIMIT+4096,MEM_COMMIT|MEM_RESERVE,PAGE_READWRITE)};
        if !memory.is_null(){let _=unsafe{VirtualFree(memory,0,MEM_RELEASE)};}
        ensure!(memory.is_null(),"worker exceeded configured memory commitment boundary");
        state.status.outcome=Some(MenuOutcome::Dismissed);state.state("done")?;return Ok(());
    }
    let instance=HINSTANCE(unsafe{GetModuleHandleW(None)?}.0);let class=w!("ZSDesk.ExtensionMenu.Worker");
    unsafe{RegisterClassW(&WNDCLASSW{hInstance:instance,lpfnWndProc:Some(procedure),lpszClassName:class,..Default::default()});}
    let watch=Box::new(Watch{request:state.request.clone(),path:path.into(),
        #[cfg(test)]command:std::cell::Cell::new(None),#[cfg(test)]menu_open:std::cell::Cell::new(false)});
    let window=unsafe{CreateWindowExW(WS_EX_TOOLWINDOW,class,w!("ZSdesk"),WS_POPUP,0,0,0,0,None,None,Some(instance),Some((&*watch as*const Watch).cast()))}?;
    struct Window(HWND);impl Drop for Window{fn drop(&mut self){let _=unsafe{DestroyWindow(self.0)};}}let _window=Window(window);
    state.status.window=window.0 as isize;unsafe{SetTimer(Some(window),1,100,None);}
    let result=(||->Result<MenuOutcome>{
        let request=state.request.clone();zsdesk_windows::shell::menu::set_context_menu_options(request.options.clone());
        #[cfg(test)]if request.probe.as_deref()==Some("dismiss"){state.allow(false)?;return Ok(MenuOutcome::Dismissed);}
        if request.mode==Mode::Properties{
            state.allow(true)?;
            let pidls=if request.pidls.is_empty(){vec![zsdesk_windows::shell::pidl::location_pidl(&zsdesk_core::model::explorer::ShellLocation::Path(request.folder.clone().context("目录缺失")?)).context("目录标识无效")?]}else{request.pidls.clone()};
            let data=zsdesk_windows::desktop::data_object(window,&pidls)?;
            unsafe{windows::Win32::UI::Shell::SHMultiFileProperties(&data,0)}?;
            return Ok(MenuOutcome::ShellInvoked);
        }
        let started=Instant::now();
        let background=if request.mode==Mode::Background{Some(zsdesk_windows::shell::pidl::item_from_pidl(&request.pidls[0])?)}else{None};
        let menu=if let Some(item)=&background{
            let folder:windows::Win32::UI::Shell::IShellFolder=unsafe{item.BindToHandler(None,&windows::Win32::UI::Shell::BHID_SFObject)}?;ShellMenu::for_folder_background(window,&folder)?
        }else if request.pidls.is_empty(){let folder=zsdesk_windows::desktop::folder_for_path(request.folder.as_deref().context("目录缺失")?)?;ShellMenu::for_folder_background(window,&folder)?}else{ShellMenu::for_items(window,&request.pidls)?};
        state.status.bind_microseconds=started.elapsed().as_micros()as u64;let started=Instant::now();
        if request.mode==Mode::Open{
            menu.populate_for_invoke(request.extended)?;state.status.populate_microseconds=started.elapsed().as_micros()as u64;
            state.allow(true)?;menu.invoke_verb(window,"open",false)?;return Ok(MenuOutcome::ShellInvoked);
        }
        menu.populate(request.rename,request.extended)?;state.status.populate_microseconds=started.elapsed().as_micros()as u64;state.allow(false)?;
        #[cfg(test)]if request.probe.as_deref()==Some("modern-owner-copy"){
            ensure!(request.options.modern,"modern owner probe requires the native Windows 11 presenter");
            let count=unsafe{GetMenuItemCount(Some(menu.hmenu()))};
            let command=(0..count).map(|index|unsafe{GetMenuItemID(menu.hmenu(),index)})
                .find(|id|menu.verb(*id).is_some_and(|verb|verb.eq_ignore_ascii_case("copy"))).context("native copy command missing")?;
            watch.command.set(Some(command));
        }
        unsafe{let _=ShowWindow(window,SW_SHOWNOACTIVATE);let _=SetForegroundWindow(window);}
        let point=POINT{x:request.point[0],y:request.point[1]};
        let outcome=if let Some(item)=&background{menu.track_background_item(window,point,item,&mut||state.allow(true))?}
            else if request.pidls.is_empty(){menu.track_folder_background(window,point,request.folder.as_deref().unwrap(),0,&mut||state.allow(true))?}
            else{menu.track_selected_before(window,point,&request.pidls,0,MenuInterceptions{rename:request.rename,open:request.open},&mut||state.allow(true))?};
        Ok(outcome)
    })();
    // Modeless property pages/extension dialogs belong to the worker. Keep their
    // COM apartment alive until the user closes them; never terminate an action
    // after its parent has granted invocation merely because a menu disappeared.
    while owned_dialogs(window){pump(50);}
    match result{
        Ok(outcome)=>state.status.outcome=Some(outcome),
        Err(error) if error.downcast_ref::<windows::core::Error>().is_some_and(|error|matches!(error.code().0 as u32,0x80004004|0x800704c7))=>state.status.outcome=Some(MenuOutcome::Dismissed),
        Err(error)=>state.status.error=Some(format!("{error:#}")),
    };
    #[cfg(test)]{state.status.probe_menu_open=watch.menu_open.get();state.status.probe_command_messages=unsafe{zs_menu_command_messages()};}
    state.state("done")?;
    let persistence_deadline=Instant::now()+Duration::from_secs(1);
    let persisted=loop{
        if !owns_clipboard(){break true;}
        match zsdesk_windows::shell::file_transfer::persist_owned_file_clipboard(window){
            Ok(persisted)=>break persisted,
            // A clipboard viewer can briefly hold the clipboard immediately
            // after Copy. Retry only publication, never the Shell command.
            // Each attempt rechecks the owning PID/STA and clipboard sequence.
            Err(error) if matches!(error.code().0 as u32,0x800401d0|0x80070005|0x800700aa)
                &&Instant::now()<persistence_deadline=>pump(25),
            Err(_)=>break false,
        }
    };
    if !persisted{
        // Virtual-file streams may be arbitrarily large. Keep their provider
        // alive instead of flushing unknown delayed formats into RAM.
        while owns_clipboard(){pump(50);}
    }
    Ok(())
}
fn owns_clipboard()->bool{
    // Extensions may publish a virtual-file provider from their own STA. The
    // process must remain alive for that provider too; only current-STA HDROP
    // snapshots qualify for bounded static conversion by file_transfer.
    unsafe{GetClipboardOwner()}.ok().is_some_and(|owner|{let mut process=0;unsafe{GetWindowThreadProcessId(owner,Some(&mut process));}process==std::process::id()})
}
fn owned_dialogs(owner:HWND)->bool{
    struct Find{owner:HWND,found:bool}let mut state=Find{owner,found:false};
    unsafe extern "system" fn each(hwnd:HWND,data:LPARAM)->windows::core::BOOL{
        let state=unsafe{&mut*(data.0 as*mut Find)};
        if hwnd!=state.owner&&unsafe{IsWindowVisible(hwnd)}.as_bool()&&unsafe{GetWindow(hwnd,GW_OWNER)}.ok()==Some(state.owner){state.found=true;return false.into();}true.into()
    }
    let _=unsafe{EnumThreadWindows(windows::Win32::System::Threading::GetCurrentThreadId(),Some(each),LPARAM(&mut state as*mut Find as isize))};state.found
}
pub fn run_worker_if_requested()->Option<Result<()>>{
    let mut args=std::env::args_os().skip(1);if args.next()?.to_str()!=Some("--context-menu-worker"){return None;}
    Some((||{let _lifetime=crate::ApplicationLifetime::new();let path=args.next().context("缺少菜单请求")?;ensure!(args.next().is_none(),"菜单参数无效");run_worker(Path::new(&path))})())
}

#[cfg(test)]mod tests{
    use super::*;
    #[test]fn menu_protocol_rejects_truncated_or_oversized_identities(){
        assert!(valid_pidl(&[0,0]));assert!(valid_pidl(&[3,0,1,0,0]));
        for invalid in [vec![],vec![1,0,0,0],vec![4,0,1],vec![0,0,9],vec![0;65537]]{assert!(!valid_pidl(&invalid));}
    }
    #[test]#[ignore="Internal subprocess entry for menu isolation regression"]
    fn native_extension_worker_entry()->Result<()>{
        #[link(name="user32")]unsafe extern "system"{fn OpenDesktopW(name:*const u16,flags:u32,inherit:i32,access:u32)->*mut core::ffi::c_void;fn SetThreadDesktop(desktop:*mut core::ffi::c_void)->i32;}
        if let Ok(name)=std::env::var("ZSDESK_MENU_TEST_DESKTOP"){
            let desktop=unsafe{OpenDesktopW(wide(&name).as_ptr(),0,0,0x000f00ff)};ensure!(!desktop.is_null()&&unsafe{SetThreadDesktop(desktop)}!=0,"private worker desktop unavailable");
        }
        let root=PathBuf::from(std::env::var_os("ZSDESK_MENU_WORKSPACE").context("workspace")?);run_worker(&root.join("request.json"))
    }
}
#[cfg(test)]#[path="context_menu_process_tests.rs"]pub(crate) mod isolation_tests;
