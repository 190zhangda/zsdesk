//! A single UI process per profile; opening another path forwards to its window.
use std::{path::Path, hash::{Hash,Hasher},collections::hash_map::DefaultHasher,time::{Duration,Instant}};
use anyhow::{Result,bail};
use windows::{core::{PCWSTR,w},Win32::{Foundation::{HANDLE,CloseHandle,GetLastError,ERROR_ALREADY_EXISTS},System::Threading::CreateMutexW,UI::WindowsAndMessaging::*}};
use zsdesk_core::model::explorer::ShellLocation;
use windows::Win32::System::DataExchange::COPYDATASTRUCT;

pub struct Instance(HANDLE);
impl Drop for Instance {fn drop(&mut self){let _=unsafe {CloseHandle(self.0)};}}
pub const OPEN_MAGIC:usize=0x5a534432;
pub const DIAGNOSTICS_MAGIC:usize=0x5a534449;
#[derive(serde::Serialize,serde::Deserialize)]
pub struct DiagnosticRequest{pub output:std::path::PathBuf,pub request_id:u64}
pub fn diagnose(profile:&Path,output:&Path)->Result<()>{
    let key=property(profile);let mut previous=None;let mut target=None;
    while let Ok(hwnd)=unsafe{FindWindowExW(None,previous,w!("ZSDesk.Native.Explorer"),None)}{
        previous=Some(hwnd);if unsafe{GetPropW(hwnd,PCWSTR(key.as_ptr()))}.0 as usize==1{target=Some(hwnd);break;}
    }
    let Some(hwnd)=target else{bail!("此配置的文件管理器未运行");};
    let request_id=std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos() as u64 ^ (u64::from(std::process::id())<<32);
    let request=DiagnosticRequest{output:output.to_path_buf(),request_id};let bytes=serde_json::to_vec(&request)?;
    let data=COPYDATASTRUCT{dwData:DIAGNOSTICS_MAGIC,cbData:bytes.len()as u32,lpData:bytes.as_ptr()as *mut _};let mut accepted=0;
    let sent=unsafe{SendMessageTimeoutW(hwnd,WM_COPYDATA,windows::Win32::Foundation::WPARAM(0),windows::Win32::Foundation::LPARAM(&data as *const _ as isize),SMTO_ABORTIFHUNG,2500,Some(&mut accepted))};
    if sent.0==0 || accepted!=1{bail!("文件管理器未接受诊断请求");}
    let deadline=Instant::now()+Duration::from_secs(5);
    while Instant::now()<deadline{
        if let Ok(bytes)=std::fs::read(output){if let Ok(value)=serde_json::from_slice::<serde_json::Value>(&bytes){if value["request_id"].as_u64()==Some(request_id){return Ok(());}}}
        std::thread::sleep(Duration::from_millis(20));
    }
    bail!("诊断报告尚未生成")
}
pub fn property(path:&Path)->Vec<u16>{
    let mut h=DefaultHasher::new();path.to_string_lossy().to_lowercase().hash(&mut h);
    format!("Local\\ZSDesk.Profile.{:016x}",h.finish()).encode_utf16().chain(Some(0)).collect()
}
pub fn enter(path:&Path,target:Option<&ShellLocation>,selection:Option<&std::ffi::OsStr>)->Result<Option<Instance>>{
    let key=property(path);
    let handle=unsafe {CreateMutexW(None,false,PCWSTR(key.as_ptr()))?};
    let existed=unsafe {GetLastError()}==ERROR_ALREADY_EXISTS;
    let guard=Instance(handle);
    if !existed{return Ok(Some(guard));}
    let deadline=Instant::now()+Duration::from_secs(3);
    loop {
        let mut previous=None;
        while let Ok(hwnd)=unsafe {FindWindowExW(None,previous,w!("ZSDesk.Native.Explorer"),None)} {
            previous=Some(hwnd);
            if unsafe {GetPropW(hwnd,PCWSTR(key.as_ptr()))}.0 as usize!=1 {continue;}
            let bytes=serde_json::to_vec(&(target,selection.map(|s|s.to_string_lossy().into_owned())))?;
            let data=COPYDATASTRUCT{dwData:OPEN_MAGIC,cbData:bytes.len() as u32,lpData:bytes.as_ptr() as *mut _};
            let mut accepted=0;
            let sent=unsafe {SendMessageTimeoutW(hwnd,WM_COPYDATA,windows::Win32::Foundation::WPARAM(0),
                windows::Win32::Foundation::LPARAM((&data as *const COPYDATASTRUCT) as isize),
                SMTO_ABORTIFHUNG,2500,Some(&mut accepted))};
            if sent.0==0||accepted!=1{bail!("已打开的 ZSDesk 暂时无法接收请求");}
            unsafe {let _=ShowWindow(hwnd,if IsIconic(hwnd).as_bool(){SW_RESTORE}else{SW_SHOW});let _=SetForegroundWindow(hwnd);}
            return Ok(None);
        }
        if Instant::now()>=deadline {bail!("ZSDesk 正在启动，请稍后重试");}
        std::thread::sleep(Duration::from_millis(60));
    }
}
pub struct OpenRequest{pub location:ShellLocation,pub selection:Option<std::ffi::OsString>,pub selections:Vec<std::ffi::OsString>}
