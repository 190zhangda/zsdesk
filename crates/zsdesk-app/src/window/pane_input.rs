//! Bounded input tickets bind delayed actions to a specific pane and tab.
use super::*;
use std::sync::{Mutex,OnceLock,atomic::{AtomicUsize,Ordering}};
pub(super) const MESSAGE:u32=WM_APP+50;
const LIMIT:usize=128;
static NEXT:AtomicUsize=AtomicUsize::new(1);
static PENDING:OnceLock<Mutex<HashMap<usize,Envelope>>>=OnceLock::new();

#[derive(Clone,Copy,Debug,PartialEq,Eq)]
pub(super) struct Identity {pub source:usize,pub tab:u64,pub epoch:u64}
pub(super) enum Intent {Action(usize),Wheel(i32),OpenFolder(PathBuf),NavigateFolder(ShellLocation),SubmitAddress(String)}
struct Envelope {owner:isize,identity:Identity,intent:Intent}

pub(super) fn post(owner:HWND,identity:Identity,intent:Intent)->bool {
    let token=NEXT.fetch_add(1,Ordering::Relaxed).max(1);
    let pending=PENDING.get_or_init(Default::default);
    {
        let mut pending=pending.lock().unwrap_or_else(|e|e.into_inner());
        if pending.len()>=LIMIT{return false;}
        pending.insert(token,Envelope{owner:owner.0 as isize,identity,intent});
    }
    if unsafe{PostMessageW(Some(owner),MESSAGE,WPARAM(token),LPARAM(0))}.is_ok(){return true;}
    pending.lock().unwrap_or_else(|e|e.into_inner()).remove(&token);false
}

pub(super) fn discard(owner:HWND) {
    if let Some(pending)=PENDING.get(){pending.lock().unwrap_or_else(|e|e.into_inner()).retain(|_,envelope|envelope.owner!=owner.0 as isize);}
}

pub(super) fn identity(owner:HWND,source:usize)->Option<Identity> {
    let pointer=unsafe{GetWindowLongPtrW(owner,GWLP_USERDATA)}as *const Slot;
    if pointer.is_null(){return None;}
    unsafe{&*pointer}.try_borrow().ok().and_then(|slot|slot.as_ref().and_then(|state|state.identity_for_source(source)))
}

pub(super) fn capture_folder_open(owner:HWND,source:usize,view:HWND)->Option<zsdesk_windows::shell::FolderOpenCallback> {
    let pointer=unsafe{GetWindowLongPtrW(owner,GWLP_USERDATA)}as *const Slot;
    if pointer.is_null(){return None;}
    let snapshot={let slot=unsafe{&*pointer}.try_borrow().ok()?;let state=slot.as_ref()?;
        if state.source_for_view(view)!=Some(source){return None;}
        state.identity_for_source(source)?
    };
    Some(Box::new(move|location|{
        if !post(owner,snapshot,Intent::NavigateFolder(location)) {
            diagnostics::record("navigation","native-open-mailbox-full",Some(0x800700aau32 as i32));
        }
    }))
}

impl WindowState {
    pub(super) fn identity_for_source(&self,source:usize)->Option<Identity> {
        if source==0{return None;}
        if source==self.browser_source {
            return self.session.tabs.get(self.session.active_tab).map(|tab|Identity{source,tab:tab.id,epoch:self.navigation_epoch});
        }
        if self.session.ui.dual_pane && source==self.other_source() {
            return self.other_identity();
        }
        None
    }

    pub(super) fn queue_pane_action(&self,source:usize,action:usize)->bool {
        self.identity_for_source(source).is_some_and(|identity|post(self.hwnd,identity,Intent::Action(action)))
    }

    pub(super) fn receive_pane_input(&mut self,token:usize) {
        let envelope=PENDING.get().and_then(|pending|pending.lock().unwrap_or_else(|e|e.into_inner()).remove(&token));
        let Some(envelope)=envelope.filter(|envelope|envelope.owner==self.hwnd.0 as isize)else{return;};
        if self.identity_for_source(envelope.identity.source)!=Some(envelope.identity){return;}
        match envelope.intent {
            Intent::Action(action)=>self.pane_action(envelope.identity.source,action),
            Intent::Wheel(delta)=>self.pane_wheel(envelope.identity.source,delta),
            Intent::OpenFolder(path)=>self.pane_open_folder(envelope.identity.source,path),
            Intent::NavigateFolder(location)=>self.pane_navigate_folder(envelope.identity.source,location),
            Intent::SubmitAddress(text)=>if envelope.identity.source==self.browser_source {
                address_input::delivered(self.address_edit);
                diagnostics::record("navigation","address-submit-dispatched",None);
                unsafe{self.commit_address_text(&text);}
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stale_tab_and_navigation_inputs_are_rejected_without_consuming_new_state() {
        let mut state=WindowState::new(ExplorerSession::default(),PathBuf::new(),None,false);state.browser_source=17;
        let first=state.identity_for_source(17).unwrap();
        state.session.tabs.push(ExplorerTab::new(2,ShellLocation::ThisPc));state.session.active_tab=1;
        assert_ne!(state.identity_for_source(17),Some(first));
        state.session.active_tab=0;state.navigation_epoch+=1;
        assert_ne!(state.identity_for_source(17),Some(first));
        assert!(state.identity_for_source(18).is_none());
    }
}
