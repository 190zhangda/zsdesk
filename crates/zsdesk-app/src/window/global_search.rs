use super::*;

impl WindowState{
    pub(super) fn write_diagnostics(&self,request:&crate::instance::DiagnosticRequest)->anyhow::Result<()>{
        if request.output.exists(){
            let existing:serde_json::Value=serde_json::from_slice(&std::fs::read(&request.output)?)?;
            anyhow::ensure!(existing["schema"]=="ZSDesk.Diagnostics.v1","输出文件不是既有诊断报告");
        }
        let paths=self.browser.as_ref().map(|b|b.item_paths_limit(128)).unwrap_or_default();
        let selected=self.browser.as_ref().map(|b|b.selection_paths_limit(128)).unwrap_or_default();
        let status=self.browser.as_ref().map(|b|b.status()).unwrap_or_default();
        let value=serde_json::json!({"schema":"ZSDesk.Diagnostics.v1","request_id":request.request_id,
            "pid":std::process::id(),"version":env!("CARGO_PKG_VERSION"),"location":self.current_location(),
            "navigation_pending":self.pending.is_some(),"items":status.items,"selected":status.selected,
            "item_paths":paths,"selected_paths":selected,"fast_generation":self.fast_generation,"fast_found":self.fast_found,
            "search_progress":self.search_progress,"message":self.status,"icon_cache_items":self.icons.len(),
            "surface_bytes":self.surface.as_ref().map(|s|u64::from(s.width.max(0)as u32)*u64::from(s.height.max(0)as u32)*4).unwrap_or(0),
            "ui":self.session.ui,"menu":zsdesk_windows::shell::menu_diagnostics()});
        if let Some(parent)=request.output.parent(){std::fs::create_dir_all(parent)?;}
        let temp=request.output.with_extension("diagnostic.tmp");
        std::fs::write(&temp,serde_json::to_vec_pretty(&value)?)?;std::fs::rename(temp,&request.output)?;Ok(())
    }
    pub(super) fn cancel_fast_search(&mut self){
        if let Some(engine)=&self.fast_search{engine.cancel();}
        self.fast_generation=0;self.fast_events.clear();self.search_progress=None;
    }
    pub(super) fn reset_browser_host(&mut self)->windows::core::Result<()>{
        self.cancel_fast_search();self.browser=None;self.browser_role=BrowserRole::Fresh;
        let panes=Panes{preview:self.session.ui.show_preview,details:self.session.ui.show_details};
        let browser=unsafe{ExplorerBrowserHost::create(self.hwnd,win_rect(self.layout.browser),self.modern,panes)?};
        browser.set_context_menu_options(crate::context_preferences::native(&self.session.ui.context_menu));
        self.browser=Some(browser);self.theme_children();Ok(())
    }
    pub(super) fn start_fast_search(&mut self,query:&str){
        if self.fast_search.is_none(){
            match crate::fast_search::FastSearch::new(self.config_path.with_file_name("search-index.sqlite"),self.hwnd,WM_FAST_SEARCH){
                Ok(engine)=>self.fast_search=Some(engine),Err(error)=>{self.show_message(format!("无法建立文件名索引：{error}"));return;}
            }
        }
        let preferred=self.search_scope.as_ref().and_then(|s|s.as_path().map(PathBuf::from));
        self.fast_found=0;self.fast_events.clear();
        self.fast_generation=self.fast_search.as_ref().unwrap().start(query,preferred);
        self.search_progress=Some("正在搜索全部本地磁盘…".into());
        self.relayout();self.position_children();self.invalidate();
    }
    pub(super) fn poll_fast_search(&mut self){
        if self.fast_generation==0 || !matches!(self.current_location(),ShellLocation::Search{scope:None,..}){return;}
        if self.fast_events.is_empty(){if let Some(engine)=&self.fast_search{self.fast_events.extend(engine.poll());}}
        let Some(event)=self.fast_events.pop_front()else{return;};
        if event.generation==self.fast_generation{
            if !event.paths.is_empty(){
                if let Some(browser)=&self.browser{match browser.append_search_paths(&event.paths){
                    Ok(added)=>self.fast_found+=added,
                    Err(error) if error.code().0 as u32==0x8000000A=>{
                        self.fast_events.push_front(event);unsafe{SetTimer(Some(self.hwnd),TIMER_FAST_SEARCH,30,None);}return;
                    },
                    Err(error)=>{self.show_message(format!("无法显示搜索结果：{error}"));}
                }}
            }
            self.search_progress=Some(progress_text(&event,self.fast_found));
            self.refresh_status();self.relayout();self.position_children();self.invalidate_rect(self.layout.status);
        }
        // Drain small batches after yielding to keyboard, painting and menus.
        if !self.fast_events.is_empty(){post(self.hwnd,WM_FAST_SEARCH,0,0);}
        else if let Some(engine)=&self.fast_search{let events=engine.poll();if !events.is_empty(){self.fast_events.extend(events);post(self.hwnd,WM_FAST_SEARCH,0,0);}}
    }
}
fn progress_text(event:&crate::fast_search::SearchEvent,found:usize)->String{
    if let Some(error)=&event.error{return format!("搜索未完成：{error}");}
    let mut text=if event.truncated{format!("已显示前 {found} 条结果，请补充关键字")}
    else if event.scanning{format!("找到 {found} 条 · 正在更新文件名索引（已扫描 {} 项）",event.scanned)}
    else if event.finished{format!("找到 {found} 条 · 索引 {} 项 · F5 更新",event.indexed)}
    else{format!("正在查询 · 已显示 {found} 条")};
    if event.denied>0{text.push_str(&format!(" · 跳过 {} 处受限位置",event.denied));}
    if event.skipped_reparse>0{text.push_str(&format!(" · {} 处目录链接未跟随",event.skipped_reparse));}
    text
}
