//! Per-view erase colors without replacing the Shell's theme/selection renderer.
use std::{cell::Cell,rc::Rc};
use windows::{core::BOOL,Win32::{Foundation::{HWND,LPARAM,LRESULT,WPARAM,COLORREF,RECT},
    Graphics::Gdi::{CreateSolidBrush,DeleteObject,FillRect,HBRUSH,HDC,GetSysColorBrush,COLOR_WINDOW},
    UI::{Shell::{IShellView,SetWindowSubclass,GetWindowSubclass,RemoveWindowSubclass,DefSubclassProc},
        WindowsAndMessaging::{EnumChildWindows,GetClassNameW,GetClientRect,WM_ERASEBKGND,WM_NCDESTROY,WM_PARENTNOTIFY,WM_CREATE}}}};

const SUBCLASS:usize=0x5a534247;
#[derive(Clone,Copy)]
pub(super) struct Colors{pub background:u32,pub high_contrast:bool}
struct State{colors:Rc<Cell<Colors>>,brush:HBRUSH,last:Option<u32>}
impl Drop for State{fn drop(&mut self){if !self.brush.0.is_null(){let _=unsafe{DeleteObject(self.brush.into())};}}}
fn supported(hwnd:HWND)->bool{
    let mut name=[0u16;96];let count=unsafe{GetClassNameW(hwnd,&mut name)}.max(0)as usize;
    matches!(String::from_utf16_lossy(&name[..count]).as_str(),"SHELLDLL_DefView"|"DirectUIHWND"|"SysListView32"|"ExplorerBrowserControl")
}
fn install_one(hwnd:HWND,colors:Rc<Cell<Colors>>){
    let mut old=0usize;
    if unsafe{GetWindowSubclass(hwnd,Some(procedure),SUBCLASS,Some(&mut old))}.as_bool(){
        if old!=0 {unsafe{(*(old as*mut State)).colors=colors;}}return;
    }
    let pointer=Box::into_raw(Box::new(State{colors,brush:HBRUSH::default(),last:None}));
    if !unsafe{SetWindowSubclass(hwnd,Some(procedure),SUBCLASS,pointer as usize)}.as_bool(){unsafe{drop(Box::from_raw(pointer));}}
}
unsafe extern "system" fn enumerate(hwnd:HWND,lp:LPARAM)->BOOL{
    if supported(hwnd){install_one(hwnd,unsafe{&*(lp.0 as*const Rc<Cell<Colors>>)}.clone());}true.into()
}
fn install_children(hwnd:HWND,colors:&Rc<Cell<Colors>>){
    let _=unsafe{EnumChildWindows(Some(hwnd),Some(enumerate),LPARAM((colors as*const Rc<Cell<Colors>>)as isize))};
}
pub(super) fn install(view:&IShellView,colors:Rc<Cell<Colors>>){
    let Ok(hwnd)=(unsafe{view.GetWindow()})else{return;};
    install_one(hwnd,colors.clone());install_children(hwnd,&colors);
}
unsafe extern "system" fn procedure(hwnd:HWND,msg:u32,wp:WPARAM,lp:LPARAM,id:usize,data:usize)->LRESULT{
    if msg==WM_NCDESTROY{
        let _=unsafe{RemoveWindowSubclass(hwnd,Some(procedure),id)};
        unsafe{drop(Box::from_raw(data as*mut State));}
        return unsafe{DefSubclassProc(hwnd,msg,wp,lp)};
    }
    if msg==WM_ERASEBKGND && wp.0!=0{
        let state=unsafe{&mut*(data as*mut State)};let colors=state.colors.get();
        let brush=if colors.high_contrast{unsafe{GetSysColorBrush(COLOR_WINDOW)}}else{
            if state.last!=Some(colors.background){
                let next=unsafe{CreateSolidBrush(COLORREF(colors.background))};
                if !next.0.is_null(){let old=std::mem::replace(&mut state.brush,next);state.last=Some(colors.background);
                    if !old.0.is_null(){let _=unsafe{DeleteObject(old.into())};}}
            }state.brush
        };
        let mut rect=RECT::default();
        if !brush.0.is_null() && unsafe{GetClientRect(hwnd,&mut rect)}.is_ok()
            && unsafe{FillRect(HDC(wp.0 as*mut _),&rect,brush)}!=0{return LRESULT(1);}
    }
    if msg==WM_PARENTNOTIFY && (wp.0&0xffff)as u32==WM_CREATE{
        let colors=unsafe{&*(data as*const State)}.colors.clone();let child=HWND(lp.0 as*mut _);
        if supported(child){install_one(child,colors.clone());}install_children(child,&colors);
    }
    unsafe{DefSubclassProc(hwnd,msg,wp,lp)}
}

#[cfg(test)]mod tests{
    use super::*;
    use windows::Win32::{Graphics::Gdi::{CreateCompatibleDC,CreateCompatibleBitmap,SelectObject,DeleteDC,GetPixel,GetDC,ReleaseDC},
        UI::WindowsAndMessaging::{CreateWindowExW,DestroyWindow,SendMessageW,WS_POPUP,WINDOW_EX_STYLE}};
    #[test]
    fn native_view_erase_uses_current_palette_without_changing_theme(){
        let hwnd=unsafe{CreateWindowExW(WINDOW_EX_STYLE::default(),windows::core::w!("STATIC"),windows::core::w!("erase test"),WS_POPUP,0,0,8,8,None,None,None,None)}.unwrap();
        let colors=Rc::new(Cell::new(Colors{background:0x202020,high_contrast:false}));install_one(hwnd,colors.clone());
        let screen=unsafe{GetDC(None)};let dc=unsafe{CreateCompatibleDC(Some(screen))};let bitmap=unsafe{CreateCompatibleBitmap(screen,8,8)};
        unsafe{ReleaseDC(None,screen);}
        let previous=unsafe{SelectObject(dc,bitmap.into())};
        for value in [0x202020,0xeeeeee]{
            colors.set(Colors{background:value,high_contrast:false});
            assert_eq!(unsafe{SendMessageW(hwnd,WM_ERASEBKGND,Some(WPARAM(dc.0 as usize)),Some(LPARAM(0)))}.0,1);
            assert_eq!(unsafe{GetPixel(dc,2,2)}.0,value);
        }
        unsafe{SelectObject(dc,previous);let _=DeleteObject(bitmap.into());let _=DeleteDC(dc);let _=DestroyWindow(hwnd);}
    }
}
