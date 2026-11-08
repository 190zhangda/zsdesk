//! Reversible forwarding of newly created, single-tab Explorer folder windows.
//! Existing windows and unconfirmed navigations are never closed.
use std::{collections::{HashMap,HashSet},path::{Path,PathBuf},sync::{Arc,atomic::{AtomicBool,Ordering}},thread::{self,JoinHandle},time::{Duration,Instant}};
use anyhow::{ensure,Context,Result};
use windows::{core::{Interface,PCWSTR},Win32::{Foundation::{HWND,LPARAM,WPARAM},System::{Com::{CoCreateInstance,CoInitializeEx,CoUninitialize,CLSCTX_LOCAL_SERVER,COINIT_APARTMENTTHREADED},DataExchange::COPYDATASTRUCT,Variant::VARIANT},UI::{Shell::{IShellWindows,IWebBrowser2,IShellFolderViewDual,Folder2,ShellWindows},WindowsAndMessaging::*}}};
use windows::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled;

pub(crate) fn same_path(a:&Path,b:&Path)->bool{
    use std::os::windows::ffi::OsStrExt;
    let units=|p:&Path|p.as_os_str().encode_wide().map(|v|if v==47{92}else{v}).collect::<Vec<_>>();
    let mut a=units(a);let mut b=units(b);while a.len()>3&&a.last()==Some(&92){a.pop();}while b.len()>3&&b.last()==Some(&92){b.pop();}
    (unsafe{windows::Win32::Globalization::CompareStringOrdinal(&a,&b,true)})==windows::Win32::Globalization::CSTR_EQUAL
}
pub fn owns_integration(profile:&Path)->bool{
    use zsdesk_windows::integration::{self,Integration};
    std::path::absolute(profile).ok().zip(std::path::absolute(crate::storage::config_path()).ok()).is_some_and(|(a,b)|same_path(&a,&b))
        && integration::current()==Integration::Default
        && std::env::current_exe().is_ok_and(|exe|integration::win_e_enabled(&exe))
}
struct Apartment;
impl Drop for Apartment{fn drop(&mut self){unsafe{CoUninitialize();}}}
#[derive(Clone)]
struct Snapshot{window:HWND,pid:u32,folder:PathBuf,selected:Vec<PathBuf>,browser:IWebBrowser2,this_pc:bool}
impl Snapshot{
    fn equivalent(&self,other:&Self)->bool{self.window==other.window&&self.pid==other.pid&&self.this_pc==other.this_pc&&same_path(&self.folder,&other.folder)
        && self.selected.len()==other.selected.len()&&self.selected.iter().all(|a|other.selected.iter().any(|b|same_path(a,b)))}
}
fn windows(shell:&IShellWindows)->Result<Vec<Snapshot>>{
    let mut result=Vec::new();let mut counts=HashMap::new();
    for index in 0..unsafe{shell.Count()?}.clamp(0,128){
        let Ok(dispatch)=(unsafe{shell.Item(&VARIANT::from(index))})else{continue;};
        let Ok(browser)=dispatch.cast::<IWebBrowser2>()else{continue;};
        let Ok(handle)=(unsafe{browser.HWND()})else{continue;};let window=HWND(handle.0 as*mut _);
        *counts.entry(window.0 as isize).or_insert(0usize)+=1;
        let mut class=[0u16;80];let n=unsafe{GetClassNameW(window,&mut class)}.max(0)as usize;
        if !matches!(String::from_utf16_lossy(&class[..n]).as_str(),"CabinetWClass"|"ExploreWClass"){continue;}
        let expected=PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(||r"C:\Windows".into())).join("explorer.exe");
        if !unsafe{browser.FullName()}.is_ok_and(|name|same_path(Path::new(&name.to_string()),&expected)){continue;}
        let mut pid=0;unsafe{GetWindowThreadProcessId(window,Some(&mut pid));}if pid==0{continue;}
        let snapshot=(||->windows::core::Result<Snapshot>{
            let view=unsafe{browser.Document()?}.cast::<IShellFolderViewDual>()?;
            let folder=unsafe{view.Folder()?}.cast::<Folder2>()?;
            let folder=PathBuf::from(unsafe{folder.Self_()?.Path()?}.to_string());
            let items=unsafe{view.SelectedItems()?};let count=unsafe{items.Count()?};
            if !(0..=128).contains(&count){return Err(windows::core::Error::from_hresult(windows::core::HRESULT(0x80070057u32 as i32)));}
            let mut selected=Vec::with_capacity(count as usize);
            for i in 0..count{selected.push(PathBuf::from(unsafe{items.Item(&VARIANT::from(i))?.Path()?}.to_string()));}
            let this_pc=folder.to_string_lossy().eq_ignore_ascii_case("::{20D04FE0-3AEA-1069-A2D8-08002B30309D}");
            Ok(Snapshot{window,pid,folder,selected,browser:browser.clone(),this_pc})
        })();
        if let Ok(snapshot)=snapshot{if (snapshot.this_pc&&snapshot.selected.is_empty())||(snapshot.folder.is_absolute()&&snapshot.selected.iter().all(|p|p.parent().is_some_and(|parent|same_path(parent,&snapshot.folder)))){result.push(snapshot);}}
    }
    result.retain(|snapshot|counts.get(&(snapshot.window.0 as isize))==Some(&1));Ok(result)
}
fn find_target(profile:&Path)->Option<HWND>{
    let property=crate::instance::property(profile);let mut previous=None;
    while let Ok(hwnd)=unsafe{FindWindowExW(None,previous,windows::core::w!("ZSDesk.Native.Explorer"),None)}{
        previous=Some(hwnd);if unsafe{GetPropW(hwnd,PCWSTR(property.as_ptr()))}.0 as usize==1{return Some(hwnd);}
    }None
}
fn send(window:HWND,tag:usize,packet:&[u8])->bool{
    let data=COPYDATASTRUCT{dwData:tag,cbData:packet.len()as u32,lpData:packet.as_ptr()as*mut _};let mut response=0;
    unsafe{SendMessageTimeoutW(window,WM_COPYDATA,WPARAM(0),LPARAM(&data as*const _ as isize),SMTO_ABORTIFHUNG,500,Some(&mut response))}.0!=0&&response==1
}
fn forward(profile:&Path,executable:&Path,snapshot:&Snapshot)->Result<()>{
    let packet=if snapshot.this_pc{serde_json::to_vec(&(Some(zsdesk_core::model::explorer::ShellLocation::ThisPc),None::<String>))?}
        else{crate::reveal::encode(&snapshot.folder,&snapshot.selected).context("文件位置请求超出支持范围")?};
    if find_target(profile).is_none(){
        use std::os::windows::process::CommandExt;
        let mut command=std::process::Command::new(executable);
        command.arg("--profile-dir").arg(profile.parent().context("配置目录缺失")?);
        if snapshot.this_pc{command.arg("--this-pc");}else{command.arg(&snapshot.folder);}
        command
            .creation_flags(0x08000000).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn()?;
    }
    let deadline=Instant::now()+Duration::from_secs(8);
    let target=loop{if let Some(target)=find_target(profile){break target;}ensure!(Instant::now()<deadline,"文件管理窗口未就绪");thread::sleep(Duration::from_millis(50));};
    ensure!(send(target,if snapshot.this_pc{crate::instance::OPEN_MAGIC}else{crate::reveal::COPYDATA_TAG},&packet),"目标窗口未接受文件位置请求");
    let deadline=Instant::now()+Duration::from_secs(4);
    while !send(target,if snapshot.this_pc{crate::reveal::VERIFY_THIS_PC}else{crate::reveal::VERIFY_TAG},if snapshot.this_pc{&[]}else{&packet}){ensure!(Instant::now()<deadline,"导航或选中状态未确认，保留系统窗口");thread::sleep(Duration::from_millis(75));}
    // A fresh snapshot prevents closing a window the user has already navigated
    // elsewhere, turned into multiple tabs, or placed behind a modal dialog.
    let shell:IShellWindows=unsafe{CoCreateInstance(&ShellWindows,None,CLSCTX_LOCAL_SERVER)}?;
    let latest=windows(&shell)?.into_iter().find(|item|item.window==snapshot.window).context("来源窗口状态已改变")?;
    ensure!(snapshot.equivalent(&latest)&&unsafe{IsWindowEnabled(snapshot.window)}.as_bool(),"来源窗口已改变，保留系统窗口");
    let foreground=unsafe{GetForegroundWindow()}==snapshot.window;
    if foreground{crate::instance::request_activation(target);}
    unsafe{snapshot.browser.Quit()}?;Ok(())
}

pub struct Bridge{stop:Arc<AtomicBool>,worker:Option<JoinHandle<()>>}
impl Bridge{
    pub fn start(profile:&Path)->Result<Option<Self>>{
        if !same_path(&std::path::absolute(profile)?,&std::path::absolute(crate::storage::config_path())?){return Ok(None);}
        let profile=profile.to_path_buf();let stop=Arc::new(AtomicBool::new(false));let signal=stop.clone();
        let worker=thread::Builder::new().name("zsdesk-explorer-forwarding".into()).stack_size(512*1024).spawn(move||{
            if unsafe{CoInitializeEx(None,COINIT_APARTMENTTHREADED)}.is_err(){return;}let _apartment=Apartment;
            let mut shell:Option<IShellWindows>=None;let mut baseline=HashSet::new();let mut candidates:HashMap<isize,(Snapshot,Instant)>=HashMap::new();
            let mut active=false;let mut refresh=Instant::now()-Duration::from_secs(3);let mut forwarded=0u64;
            while !signal.load(Ordering::Acquire){
                if refresh.elapsed()>=Duration::from_secs(2){
                    let enabled=owns_integration(&profile)&&crate::storage::load(&profile).is_ok_and(|session|session.ui.forward_unhandled_windows);
                    if enabled!=active{active=enabled;shell=None;baseline.clear();candidates.clear();write_status(&profile,active,forwarded,None);}
                    refresh=Instant::now();
                }
                if active{
                    if shell.is_none(){
                        if let Ok(value)=unsafe{CoCreateInstance::<_,IShellWindows>(&ShellWindows,None,CLSCTX_LOCAL_SERVER)}{
                            if let Ok(items)=windows(&value){baseline.extend(items.iter().map(|item|item.window.0 as isize));}
                            shell=Some(value);
                        }
                    }else if let Some(shell)=&shell{
                        if let Ok(items)=windows(shell){
                            let present:HashSet<_>=items.iter().map(|item|item.window.0 as isize).collect();
                            baseline.retain(|window|unsafe{IsWindow(Some(HWND(*window as*mut _)))}.as_bool());candidates.retain(|window,_|present.contains(window));
                            for snapshot in items{
                                let key=snapshot.window.0 as isize;
                                if baseline.contains(&key)||!unsafe{IsWindowVisible(snapshot.window)}.as_bool()||!unsafe{IsWindowEnabled(snapshot.window)}.as_bool(){continue;}
                                match candidates.get(&key){
                                    Some((old,since)) if old.equivalent(&snapshot)&&since.elapsed()>=Duration::from_millis(600)=>{
                                        baseline.insert(key);candidates.remove(&key);
                                        let result=std::env::current_exe().map_err(anyhow::Error::from).and_then(|exe|forward(&profile,&exe,&snapshot));
                                        match result{Ok(())=>{forwarded+=1;write_status(&profile,true,forwarded,None);},Err(error)=>write_status(&profile,true,forwarded,Some(&format!("{error:#}")))}
                                    }
                                    Some((old,_)) if old.equivalent(&snapshot)=>{}
                                    _=>{candidates.insert(key,(snapshot,Instant::now()));}
                                }
                            }
                        }
                    }
                }
                for _ in 0..5{if signal.load(Ordering::Acquire){break;}thread::sleep(Duration::from_millis(100));}
            }
        })?;
        Ok(Some(Self{stop,worker:Some(worker)}))
    }
}
impl Drop for Bridge{fn drop(&mut self){self.stop.store(true,Ordering::Release);if self.worker.as_ref().is_some_and(JoinHandle::is_finished){if let Some(worker)=self.worker.take(){let _=worker.join();}}}}
fn write_status(profile:&Path,active:bool,forwarded:u64,error:Option<&str>){
    if let Ok(bytes)=serde_json::to_vec(&serde_json::json!({"schema":1,"pid":std::process::id(),"active":active,"forwarded":forwarded,"last_error":error})){
        let _=crate::quick_search::control::atomic_write(&profile.with_file_name("explorer-forwarding-status.json"),&bytes);
    }
}

#[cfg(test)] mod tests{
    use super::*;
    #[test]fn path_comparison_preserves_unicode_and_normalizes_windows_spelling(){assert!(same_path(Path::new(r"E:\资料\Äbc"),Path::new("e:/资料/äBC/")));assert!(!same_path(Path::new(r"E:\a"),Path::new(r"E:\ab")));}
    #[test]
    #[ignore="Explicitly selected owned Explorer window and test profile required"]
    fn native_forwarding_confirms_navigation_before_closing_source()->Result<()>{
        let hwnd:usize=std::env::var("ZSDESK_FORWARD_TEST_HWND")?.parse()?;
        let profile=PathBuf::from(std::env::var_os("ZSDESK_FORWARD_TEST_PROFILE").context("profile")?);
        let executable=PathBuf::from(std::env::var_os("ZSDESK_FORWARD_TEST_EXE").context("exe")?);
        let output=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("output")?);
        unsafe{CoInitializeEx(None,COINIT_APARTMENTTHREADED)}.ok()?;let _apartment=Apartment;
        let shell:IShellWindows=unsafe{CoCreateInstance(&ShellWindows,None,CLSCTX_LOCAL_SERVER)}?;
        let snapshot=windows(&shell)?.into_iter().find(|item|item.window.0 as usize==hwnd).context("test Explorer window")?;
        ensure!(snapshot.folder.starts_with(&output),"test must be inside isolated fixtures");
        let selection=snapshot.selected.clone();forward(&profile,&executable,&snapshot)?;
        let deadline=Instant::now()+Duration::from_secs(3);while unsafe{IsWindow(Some(snapshot.window))}.as_bool(){ensure!(Instant::now()<deadline,"source window remained open");thread::sleep(Duration::from_millis(30));}
        let value=serde_json::json!({"state":"passed","folder":snapshot.folder,"selected":selection,"navigation_confirmed":true,"source_closed":true});
        std::fs::write(output.join("forwarding.current.json"),serde_json::to_vec_pretty(&value)?)?;println!("{value}");Ok(())
    }
}
