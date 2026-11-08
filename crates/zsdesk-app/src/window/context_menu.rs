//! Immediate common commands with explicit, isolated extension menus.
use super::*;
use std::collections::HashMap;
use windows::{core::{Interface,Error,HRESULT},Win32::{System::{Com::CoTaskMemFree,SystemServices::SFGAO_FOLDER},UI::Shell::{IFolderView2,IPersistFolder2,IShellItemArray,IShellView,SVGIO_SELECTION}}};
use zsdesk_windows::shell::{file_transfer,menu::{self,MenuInterceptions,MenuOutcome,QuickAction,QuickMenu}};
use crate::context_menu_process::{Client,Mode as WorkerMode};

pub(super) const TIMER:usize=19;
struct Session{generation:u64,view:IShellView,pidls:Vec<Vec<u8>>,folder:Option<PathBuf>,background:Option<Vec<u8>>,client:Client}
thread_local!{static ACTIVE:RefCell<HashMap<isize,Session>>=RefCell::new(HashMap::new());}
thread_local!{static GENERATIONS:RefCell<HashMap<isize,u64>>=RefCell::new(HashMap::new());static NEXT:Cell<u64>=const{Cell::new(1)};}
fn generation(owner:HWND)->Option<u64>{GENERATIONS.with(|generations|generations.borrow().get(&(owner.0 as isize)).copied())}
fn current(owner:HWND,value:u64)->bool{generation(owner)==Some(value)}
fn failure(message:&str)->Error{Error::new(HRESULT(0x80004005u32 as i32),message)}
fn snapshot(view:&IShellView)->windows::core::Result<Vec<Vec<u8>>>{
    let folder:IFolderView2=view.cast()?;
    if unsafe{folder.ItemCount(SVGIO_SELECTION)?}==0{return Ok(Vec::new());}
    let items:IShellItemArray=unsafe{folder.Items(SVGIO_SELECTION)?};let count=unsafe{items.GetCount()?};
    if count>8192{return Err(failure("选择数量过多，请减少后重试"));}
    let mut pidls=Vec::with_capacity(count as usize);
    for index in 0..count{let item=unsafe{items.GetItemAt(index)?};pidls.push(zsdesk_windows::shell::pidl::item_pidl(&item).ok_or_else(||failure("无法读取文件标识"))?);}
    Ok(pidls)
}
fn same_selection(view:&IShellView,pidls:&[Vec<u8>],folder:&Option<PathBuf>,background:&Option<Vec<u8>>)->bool{
    snapshot(view).is_ok_and(|mut current|{let mut expected=pidls.to_vec();current.sort();expected.sort();current==expected})
        &&file_transfer::folder_path(view).ok()==*folder
        &&background.as_ref().is_none_or(|expected|background_pidl(view).is_ok_and(|value|value==*expected))
}
fn root(view:HWND)->HWND{let owner=unsafe{GetAncestor(view,GA_ROOT)};if owner.0.is_null(){view}else{owner}}
fn point(view:HWND,position:LPARAM)->POINT{
    if position.0 != -1{return POINT{x:position.0 as i16 as i32,y:(position.0>>16)as i16 as i32};}
    let mut bounds=RECT::default();let _=unsafe{GetWindowRect(view,&mut bounds)};
    POINT{x:bounds.left+36,y:bounds.top+36}
}
pub(super) fn install(){menu::set_view_menu_delegate(Some(show));}
#[cfg(test)]pub(super) fn active_pid(owner:HWND)->Option<u32>{ACTIVE.with(|active|active.borrow().get(&(owner.0 as isize)).and_then(|session|session.client.pid()))}
fn background_pidl(view:&IShellView)->windows::core::Result<Vec<u8>>{
    let folder:IFolderView2=view.cast()?;let persistent:IPersistFolder2=unsafe{folder.GetFolder()?};
    let value=unsafe{persistent.GetCurFolder()?};let bytes=zsdesk_windows::shell::pidl::pidl_bytes(value);unsafe{CoTaskMemFree(Some(value.cast()));}Ok(bytes)
}
pub(super) fn close(owner:HWND){
    let value=NEXT.with(|next|{let value=next.get();next.set(value.wrapping_add(1).max(1));value});
    GENERATIONS.with(|generations|{generations.borrow_mut().insert(owner.0 as isize,value);});
    ACTIVE.with(|active|{active.borrow_mut().remove(&(owner.0 as isize));});let _=unsafe{KillTimer(Some(owner),TIMER)};
}
pub(super) fn destroy(owner:HWND){close(owner);GENERATIONS.with(|generations|{generations.borrow_mut().remove(&(owner.0 as isize));});}
pub(super) fn show(window:HWND,view:&IShellView,position:LPARAM)->windows::core::Result<()>{
    let owner=root(window);close(owner);
    let generation=generation(owner).unwrap();
    let pidls=snapshot(view)?;let folder=file_transfer::folder_path(view).ok();let point=point(window,position);
    let background=if folder.is_none(){Some(background_pidl(view)?)}else{None};
    let options=menu::context_menu_options();
    let mut mode=WorkerMode::More;
    let targets=if pidls.is_empty()&&folder.is_none(){mode=WorkerMode::Background;background.clone().into_iter().collect()}else{pidls.clone()};
    let client=Client::spawn_extended(owner,point,targets,folder.clone(),options,MenuInterceptions{rename:true,open:true},mode,unsafe{GetKeyState(VK_SHIFT.0 as i32)}<0)
        .map_err(|error|failure(&format!("无法启动系统菜单：{error:#}")))?;
    if !current(owner,generation){return Ok(());}
    ACTIVE.with(|active|{active.borrow_mut().insert(owner.0 as isize,Session{generation,view:view.clone(),pidls,folder,background,client});});
    unsafe{SetTimer(Some(owner),TIMER,50,None)};Ok(())
}

fn open_folder(owner:HWND,pidls:&[Vec<u8>])->bool{
    if pidls.len()!=1{return false;}
    let Ok(item)=zsdesk_windows::shell::pidl::item_from_pidl(&pidls[0])else{return false;};
    if !unsafe{item.GetAttributes(SFGAO_FOLDER)}.is_ok_and(|attributes|attributes.0&SFGAO_FOLDER.0!=0){return false;}
    let Some(location)=(unsafe{zsdesk_windows::shell::location_from_shell_item(&item)})else{return false;};
    let value=Box::new(location);let pointer=Box::into_raw(value);
    if unsafe{PostMessageW(Some(owner),WM_SHELL_FOLDER_INVOKED,WPARAM(0),LPARAM(pointer as isize))}.is_err(){unsafe{drop(Box::from_raw(pointer));}return false;}
    true
}
pub(super) fn poll(owner:HWND){
    let Some(mut session)=ACTIVE.with(|active|active.borrow_mut().remove(&(owner.0 as isize)))else{let _=unsafe{KillTimer(Some(owner),TIMER)};return;};
    let generation=session.generation;let view=session.view.clone();let pidls=session.pidls.clone();let folder=session.folder.clone();let background=session.background.clone();
    let result=session.client.poll(&mut|_|current(owner,generation)&&unsafe{view.GetWindow()}.is_ok_and(|window|unsafe{IsWindow(Some(window))}.as_bool())&&same_selection(&view,&pidls,&folder,&background)&&current(owner,generation));
    let valid=unsafe{session.view.GetWindow()}.is_ok_and(|window|unsafe{IsWindow(Some(window))}.as_bool())&&same_selection(&session.view,&session.pidls,&session.folder,&session.background);
    if !current(owner,generation){return;}
    match result{
        None=>{ACTIVE.with(|active|{active.borrow_mut().insert(owner.0 as isize,session);});},
        Some(Ok(MenuOutcome::Rename)) if valid=>{if let Ok(folder)=session.view.cast::<IFolderView2>(){let _=unsafe{folder.DoRename()};}},
        Some(Ok(MenuOutcome::Open|MenuOutcome::Explore)) if valid=>{
            if !open_folder(owner,&session.pidls){
                let mut point=POINT::default();let _=unsafe{GetCursorPos(&mut point)};
                if let Ok(client)=Client::spawn(owner,point,session.pidls.clone(),session.folder.clone(),menu::context_menu_options(),MenuInterceptions::default(),WorkerMode::Open){
                    if current(owner,generation){session.client=client;ACTIVE.with(|active|{active.borrow_mut().insert(owner.0 as isize,session);});}
                }
            }
        },
        Some(Ok(MenuOutcome::ShellInvoked))=>post(owner,WM_ACTION,Command::Refresh.id()as usize,0),
        Some(Err(error))=>{eprintln!("isolated Shell menu: {error:#}");menu::report_failure(owner,HRESULT(0x80004005u32 as i32));},
        _=>{},
    }
}
