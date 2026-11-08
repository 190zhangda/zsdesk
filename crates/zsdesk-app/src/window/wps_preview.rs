//! Read-only WPS rendering fallback, used only inside the disposable preview host.
use std::{path::{Path,PathBuf},time::{Duration,Instant},mem::ManuallyDrop};
use anyhow::{bail,Context,Result};
use windows::{core::{GUID,PCWSTR,BSTR},Win32::System::{Com::*,Variant::*}};
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT, DPI_AWARENESS_CONTEXT_UNAWARE, SetThreadDpiAwarenessContext};

// WPS ET's hidden automation exporter uses the caller's DPI context when
// importing OOXML metrics. Per-monitor callers can produce compressed rows
// and columns. Keep the conversion at Office's logical 96 DPI; window code
// retains its own per-window DPI context and is restored on every exit path.
struct OfficeDpiScope(DPI_AWARENESS_CONTEXT);
impl OfficeDpiScope {
    fn enter()->Self{Self(unsafe{SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_UNAWARE)})}
}
impl Drop for OfficeDpiScope {
    fn drop(&mut self){if !self.0.0.is_null(){unsafe{SetThreadDpiAwarenessContext(self.0);}}}
}

fn wide(text:&str)->Vec<u16>{text.encode_utf16().chain(Some(0)).collect()}
fn invoke(object:&IDispatch,name:&str,flags:DISPATCH_FLAGS,mut args:Vec<VARIANT>)->Result<VARIANT>{
    let name_wide=wide(name);let mut id=0;
    unsafe {object.GetIDsOfNames(&GUID::zeroed(),&PCWSTR(name_wide.as_ptr()),1,0x409,&mut id)}.with_context(||format!("WPS 接口 {name}"))?;
    args.reverse();let mut property_id=-3;
    let params=DISPPARAMS{rgvarg:args.as_mut_ptr(),cArgs:args.len()as u32,
        rgdispidNamedArgs:if flags==DISPATCH_PROPERTYPUT{&mut property_id}else{std::ptr::null_mut()},
        cNamedArgs:if flags==DISPATCH_PROPERTYPUT{1}else{0}};
    let mut result=VARIANT::default();let mut exception=EXCEPINFO::default();
    let outcome=unsafe {object.Invoke(id,&GUID::zeroed(),0x409,flags,&params,Some(&mut result),Some(&mut exception),None)};
    if outcome.is_err(){if let Some(fill)=exception.pfnDeferredFillIn{let _=unsafe{fill(&mut exception)};}}
    let description=exception.bstrDescription.to_string();
    unsafe{ManuallyDrop::drop(&mut exception.bstrSource);ManuallyDrop::drop(&mut exception.bstrDescription);ManuallyDrop::drop(&mut exception.bstrHelpFile);}
    outcome.with_context(||format!("WPS {name}：{description}"))?;
    Ok(result)
}
fn get(object:&IDispatch,name:&str)->Result<VARIANT>{invoke(object,name,DISPATCH_PROPERTYGET,vec![])}
fn set(object:&IDispatch,name:&str,value:VARIANT)->Result<()>{invoke(object,name,DISPATCH_PROPERTYPUT,vec![value]).map(|_|())}
fn call(object:&IDispatch,name:&str,args:Vec<VARIANT>)->Result<VARIANT>{invoke(object,name,DISPATCH_METHOD,args)}
fn dispatch(value:VARIANT)->Result<IDispatch>{
    if value.vt()!=VT_DISPATCH {bail!("WPS 未返回文档对象");}
    unsafe {value.Anonymous.Anonymous.Anonymous.pdispVal.as_ref().cloned()}.context("WPS 对象为空")
}
fn count(object:&IDispatch)->Result<i32>{Ok(i32::try_from(&get(object,"Count")?)?)}
fn number(object:&IDispatch,name:&str)->Option<i32>{get(object,name).ok().and_then(|v|i32::try_from(&v).ok())}
fn missing()->VARIANT{
    VARIANT{Anonymous:VARIANT_0{Anonymous:ManuallyDrop::new(VARIANT_0_0{vt:VT_ERROR,
        Anonymous:VARIANT_0_0_0{scode:0x80020004u32 as i32},..Default::default()})}}
}

#[derive(Clone,Copy)] enum Family{Word,Sheet,Slides}
fn family(path:&Path)->Option<Family>{
    Some(match path.extension()?.to_str()?.to_ascii_lowercase().as_str(){
        "doc"|"docx"|"docm"|"dot"|"dotx"|"rtf"|"wps"=>Family::Word,
        "xls"|"xlsx"|"xlsm"|"xlsb"|"xlt"|"xltx"|"et"=>Family::Sheet,
        "ppt"|"pptx"|"pptm"|"pps"|"ppsx"|"dps"=>Family::Slides,_=>return None,
    })
}
pub(super) fn supported(path:&Path)->bool{family(path).is_some()}

/// The application is never attached through ROT/GetActiveObject. Every object
/// opened here is released before Quit, including failure paths.
struct Application {
    app:IDispatch, documents:Option<IDispatch>, document:Option<IDispatch>,
    initial:i32, family:Family, original_alerts:Option<VARIANT>, original_security:Option<VARIANT>,
    report:PathBuf,
}
impl Drop for Application{
    fn drop(&mut self){
        let mut document_closed=self.document.is_none();
        if let Some(doc)=self.document.take(){
            document_closed=if matches!(self.family,Family::Slides){call(&doc,"Close",vec![])}else{call(&doc,"Close",vec![false.into()])}.is_ok();
            drop(doc);
        }
        let empty=self.documents.as_ref().is_some_and(|docs|count(docs).ok()==Some(0));
        self.documents=None;
        let no_windows=get(&self.app,"Windows").and_then(dispatch).and_then(|windows|count(&windows)).ok()==Some(0);
        let hidden=get(&self.app,"Visible").ok().and_then(|v|bool::try_from(&v).ok())==Some(false);
        // WPS Word exposes background status; the other applications export synchronously.
        let idle=number(&self.app,"BackgroundSavingStatus").unwrap_or(0)==0 && number(&self.app,"BackgroundPrintingStatus").unwrap_or(0)==0;
        let mut quit_succeeded=false;
        if self.initial==0 && empty && no_windows && hidden && idle {
            quit_succeeded=if matches!(self.family,Family::Word){call(&self.app,"Quit",vec![0i32.into()])}else{call(&self.app,"Quit",vec![])}.is_ok();
        }else{
            if let Some(value)=self.original_alerts.take(){let _=set(&self.app,"DisplayAlerts",value);}
            if let Some(value)=self.original_security.take(){let _=set(&self.app,"AutomationSecurity",value);}
        }
        let report=serde_json::json!({"initial_documents":self.initial,"document_closed":document_closed,"no_documents":empty,"no_windows":no_windows,"hidden":hidden,"idle":idle,"quit_call_succeeded":quit_succeeded});
        if let Ok(bytes)=serde_json::to_vec(&report){let _=std::fs::write(&self.report,bytes);}
    }
}

pub(super) struct RenderedDocument{pub pdf:PathBuf}
impl Drop for RenderedDocument{fn drop(&mut self){let _=std::fs::remove_file(&self.pdf);}}

pub(super) fn render(path:&Path,workspace:&Path)->Result<RenderedDocument>{
    let _dpi=OfficeDpiScope::enter();
    let family=family(path).context("不支持此 WPS 文档类型")?;
    let (progid,collection)=match family{Family::Word=>("KWPS.Application","Documents"),Family::Sheet=>("KET.Application","Workbooks"),Family::Slides=>("KWPP.Application","Presentations")};
    let clsid=unsafe{CLSIDFromProgID(PCWSTR(wide(progid).as_ptr()))}.context("未安装 WPS 自动化组件")?;
    let app:IDispatch=unsafe{CoCreateInstance(&clsid,None,CLSCTX_LOCAL_SERVER|CLSCTX_ACTIVATE_32_BIT_SERVER)}
        .or_else(|_|unsafe{CoCreateInstance(&clsid,None,CLSCTX_LOCAL_SERVER)})
        .context("无法启动 WPS 只读渲染组件")?;
    let mut owned=Application{app,documents:None,document:None,initial:-1,family,original_alerts:None,original_security:None,report:workspace.join("wps-lifecycle.json")};
    let docs=dispatch(get(&owned.app,collection)?)?;
    owned.initial=count(&docs)?;owned.documents=Some(docs);
    if owned.initial!=0 {bail!("WPS 正在使用已有文档，未改变该实例；请使用已安装的预览插件或稍后重试");}
    owned.original_alerts=get(&owned.app,"DisplayAlerts").ok();
    owned.original_security=get(&owned.app,"AutomationSecurity").ok();
    if bool::try_from(&get(&owned.app,"Visible")?)? {set(&owned.app,"Visible",false.into())?;}
    set(&owned.app,"DisplayAlerts",if matches!(family,Family::Slides){1i32.into()}else{0i32.into()})?;
    set(&owned.app,"AutomationSecurity",3i32.into()).context("WPS 不支持禁用文档宏，已停止预览")?;
    let filename=VARIANT::from(BSTR::from(path.as_os_str().to_string_lossy().as_ref()));
    let args=match family{
        Family::Word=>vec![filename,false.into(),true.into(),false.into(),missing(),missing(),false.into(),missing(),missing(),missing(),missing(),false.into()],
        Family::Sheet=>vec![filename,0i32.into(),true.into()],
        Family::Slides=>vec![filename,true.into(),false.into(),false.into()],
    };
    owned.document=Some(dispatch(call(owned.documents.as_ref().unwrap(),"Open",args)?)?);
    if !bool::try_from(&get(owned.document.as_ref().unwrap(),"ReadOnly")?)?{bail!("WPS 未以只读方式打开文档");}
    if matches!(family,Family::Sheet){
        // Materialize hidden-workbook print metrics before exporting. These
        // reads retain the workbook's own column widths, font and page setup.
        let sheets=dispatch(get(owned.document.as_ref().unwrap(),"Worksheets")?)?;
        if count(&sheets)?>0{
            let sheet=dispatch(invoke(&sheets,"Item",DISPATCH_PROPERTYGET,vec![1i32.into()])?)?;
            let setup=dispatch(get(&sheet,"PageSetup")?)?;let _=get(&setup,"Zoom")?;
            let range=dispatch(get(&sheet,"UsedRange")?)?;let _=get(&range,"Width")?;let _=get(&range,"Height")?;
        }
    }
    let pdf=workspace.join(format!("wps-{}.pdf",std::process::id()));
    // A preview workspace is created uniquely by the parent and is never a document directory.
    if pdf.exists(){std::fs::remove_file(&pdf)?;}
    let target=VARIANT::from(BSTR::from(pdf.as_os_str().to_string_lossy().as_ref()));
    let doc=owned.document.as_ref().unwrap();
    match family{
        Family::Word=>{call(doc,"ExportAsFixedFormat",vec![target,17i32.into()])?;}
        // Supply the documented optional defaults explicitly: hidden WPS
        // workbooks can mis-scale cells if the export quality is omitted.
        Family::Sheet=>{call(doc,"ExportAsFixedFormat",vec![0i32.into(),target,0i32.into(),true.into(),false.into(),missing(),missing(),false.into()])?;}
        Family::Slides=>{call(doc,"SaveAs",vec![target,32i32.into()])?;}
    }
    let deadline=Instant::now()+Duration::from_secs(5);
    while (number(&owned.app,"BackgroundSavingStatus").unwrap_or(0)!=0 || number(&owned.app,"BackgroundPrintingStatus").unwrap_or(0)!=0) && Instant::now()<deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(owned);
    if std::fs::metadata(&pdf).map_or(true,|m|m.len()<5){bail!("WPS 未生成有效预览");}
    Ok(RenderedDocument{pdf})
}

#[cfg(test)]mod tests{
    use super::*;
    #[test]
    #[ignore="Requires installed WPS and six isolated Office fixtures; exports PDFs for content/layout verification"]
    fn native_wps_office_exports_are_read_only_and_close_cleanly()->Result<()>{
        use sha2::{Digest,Sha256};
        use windows::Win32::{System::{Ole::{OleInitialize,OleUninitialize},Threading::{OpenProcess,QueryFullProcessImageNameW,PROCESS_QUERY_LIMITED_INFORMATION,PROCESS_NAME_WIN32}},Foundation::CloseHandle,UI::{WindowsAndMessaging::{GetForegroundWindow,GetWindowThreadProcessId},HiDpi::{SetThreadDpiAwarenessContext,DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2}}};
        use std::sync::{Arc,atomic::{AtomicBool,Ordering}};
        let fixtures=PathBuf::from(std::env::var_os("ZSDESK_OFFICE_FIXTURES").context("fixtures")?);
        let output=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("output")?).join("office");
        anyhow::ensure!(fixtures.is_absolute()&&output.is_absolute(),"absolute paths required");std::fs::create_dir_all(&output)?;
        let foreground=unsafe{GetForegroundWindow()};let dpi=unsafe{SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)};
        unsafe{OleInitialize(None)}?;
        let running=Arc::new(AtomicBool::new(true));let monitor_running=running.clone();
        let monitor=std::thread::spawn(move||{
            let mut observations=Vec::new();let mut last=0;
            while monitor_running.load(Ordering::Relaxed){
                let hwnd=unsafe{GetForegroundWindow()};let value=hwnd.0 as usize;
                if value!=last{
                    last=value;let mut pid=0;unsafe{GetWindowThreadProcessId(hwnd,Some(&mut pid));}
                    let mut executable=String::new();
                    if let Ok(process)=unsafe{OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION,false,pid)}{
                        let mut buffer=[0u16;32768];let mut length=buffer.len()as u32;
                        if unsafe{QueryFullProcessImageNameW(process,PROCESS_NAME_WIN32,windows::core::PWSTR(buffer.as_mut_ptr()),&mut length)}.is_ok(){executable=Path::new(&String::from_utf16_lossy(&buffer[..length as usize])).file_name().unwrap_or_default().to_string_lossy().to_lowercase();}
                        let _=unsafe{CloseHandle(process)};
                    }
                    observations.push(serde_json::json!({"hwnd":value,"pid":pid,"executable":executable}));
                }
                std::thread::sleep(Duration::from_millis(20));
            }observations
        });
        let result=(||->Result<Vec<serde_json::Value>>{
            let mut reports=Vec::new();
            for (name,extension) in [("文档 预览.docx","docx"),("文档 预览.doc","doc"),("表格 预览.xlsx","xlsx"),("表格 预览.xls","xls"),("演示 预览.pptx","pptx"),("演示 预览.ppt","ppt")]{
                let source=fixtures.join(name);let before=Sha256::digest(std::fs::read(&source)?);
                let rendered=render(&source,&output)?;let pdf=output.join(format!("{extension}.pdf"));std::fs::copy(&rendered.pdf,&pdf)?;drop(rendered);
                let lifecycle:serde_json::Value=serde_json::from_slice(&std::fs::read(output.join("wps-lifecycle.json"))?)?;
                anyhow::ensure!(lifecycle["quit_call_succeeded"]==true&&lifecycle["no_documents"]==true&&lifecycle["no_windows"]==true,"WPS cleanup failed for {extension}: {lifecycle}");
                anyhow::ensure!(before==Sha256::digest(std::fs::read(&source)?),"source changed: {extension}");
                reports.push(serde_json::json!({"format":extension,"pdf_bytes":std::fs::metadata(pdf)?.len(),"source_unchanged":true,"lifecycle":lifecycle}));
            }Ok(reports)
        })();
        unsafe{OleUninitialize();if !dpi.0.is_null(){SetThreadDpiAwarenessContext(dpi);}}
        running.store(false,Ordering::Relaxed);let observations=monitor.join().map_err(|_|anyhow::anyhow!("foreground monitor failed"))?;
        let unchanged=foreground==unsafe{GetForegroundWindow()};
        let no_office_activation=!observations.iter().any(|v|matches!(v["executable"].as_str(),Some("wps.exe"|"et.exe"|"wpp.exe")));
        let report=serde_json::json!({"state":if result.is_ok()&&no_office_activation{"passed"}else{"failed"},"results":result.as_ref().map_err(|e|format!("{e:#}")),"foreground_unchanged":unchanged,"no_office_activation":no_office_activation,"foreground_observations":observations});
        std::fs::write(output.join("verification.current.json"),serde_json::to_vec_pretty(&report)?)?;println!("{report}");result?;anyhow::ensure!(no_office_activation,"hidden Office activated a window");Ok(())
    }
}
