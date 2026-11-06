//! Opt-in native rendering regression with an isolated profile and offscreen windows.
use super::*;
use std::{fs, path::Path, thread, time::{Duration, Instant}};
use anyhow::ensure;
use serde_json::{json, Value};
use windows::{core::Interface, Win32::UI::Shell::{IFolderView2, IShellView,
    FVM_DETAILS, SVGIO_ALLVIEW, SVGIO_SELECTION, SVSI_DESELECT, SVSI_SELECT,
    SVSI_FOCUSED, SVSI_DESELECTOTHERS}};

#[link(name = "user32")]
unsafe extern "system" { fn PrintWindow(window: HWND, dc: HDC, flags: u32) -> i32; }

struct Apartment;
impl Drop for Apartment { fn drop(&mut self) { unsafe { OleUninitialize(); } } }
struct OwnedWindow(HWND);
impl Drop for OwnedWindow {
    fn drop(&mut self) { unsafe { let _ = ShowWindow(self.0, SW_HIDE); let _ = DestroyWindow(self.0); } }
}

fn pump(milliseconds: u64) {
    let end = Instant::now() + Duration::from_millis(milliseconds);
    while Instant::now() < end {
        for _ in 0..512 {
            let mut message = MSG::default();
            if !unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() { break; }
            if message.message != WM_QUIT {
                unsafe { let _ = TranslateMessage(&message); DispatchMessageW(&message); }
            }
        }
        thread::sleep(Duration::from_millis(4));
    }
}

fn view(slot: &Slot) -> Result<IShellView> {
    let borrowed = slot.borrow();
    let state = borrowed.as_ref().context("window state")?;
    unsafe { state.browser.as_ref().context("browser")?.current_view() }.map_err(Into::into)
}

fn wait_navigation(slot: &Slot) -> Result<()> {
    let start = Instant::now();
    loop {
        pump(40);
        let ready = slot.borrow().as_ref().is_some_and(|state| state.pending.is_none() && state.shown_tab.is_some());
        if ready && view(slot)?.cast::<IFolderView2>().is_ok() { return Ok(()); }
        ensure!(start.elapsed() < Duration::from_secs(12), "navigation timeout");
    }
}

fn fixture(root: &Path) -> Result<()> {
    fs::create_dir_all(root)?;
    for name in ["01-alpha.txt", "02-unicode-文件.txt", "03-space name.txt"] {
        let file = root.join(name);
        if file.exists() { ensure!(fs::read(&file)? == b"ZSDesk selection regression fixture", "fixture changed"); }
        else { fs::write(&file, b"ZSDesk selection regression fixture")?; }
    }
    Ok(())
}

fn clear_selection(shell: &IShellView) -> Result<()> {
    let folder: IFolderView2 = shell.cast()?;
    for index in 0..unsafe { folder.ItemCount(SVGIO_ALLVIEW)? } {
        unsafe { folder.SelectItem(index, SVSI_DESELECT.0 as u32)?; }
    }
    Ok(())
}

fn select(shell: &IShellView, index: i32, replace: bool) -> Result<()> {
    let folder: IFolderView2 = shell.cast()?;
    let flags = SVSI_SELECT.0 | SVSI_FOCUSED.0 | if replace { SVSI_DESELECTOTHERS.0 } else { 0 };
    unsafe { folder.SelectItem(index, flags as u32)?; }
    Ok(())
}

fn capture(shell: &IShellView, root: &Path, name: &str) -> Result<(Value, Vec<u8>)> {
    pump(160);
    let window = unsafe { shell.GetWindow()? };
    let mut rect = RECT::default();
    unsafe { GetClientRect(window, &mut rect)?; }
    let (width, height) = (rect.right, rect.bottom);
    ensure!(width > 100 && height > 100 && width * height < 4_000_000, "unexpected view bounds");
    let surface = Surface::new(width, height).context("capture surface")?;
    let bytes = unsafe { std::slice::from_raw_parts_mut(surface.bits, (width * height * 4) as usize) };
    bytes.fill(127);
    ensure!(unsafe { PrintWindow(window, surface.dc, 2) } != 0, "PrintWindow failed");
    for pixel in bytes.chunks_exact_mut(4) { pixel[3] = 255; }
    let pixels = bytes.to_vec();
    fs::write(root.join(format!("{name}.bgra")), &pixels)?;
    let folder: IFolderView2 = shell.cast()?;
    let record = json!({"frame": name, "width": width, "height": height,
        "selected": unsafe { folder.ItemCount(SVGIO_SELECTION)? },
        "focus": unsafe { GetFocus() }.0 as usize, "view_window": window.0 as usize});
    Ok((record, pixels))
}

// A focus border or text-color change is insufficient: require a rectangular fill.
fn filled_rows(before: &[u8], after: &[u8], width: usize) -> usize {
    before.chunks_exact(width * 4).zip(after.chunks_exact(width * 4)).filter(|(a, b)| {
        let mut run = 0;
        let mut previous = None;
        for (left, right) in a.chunks_exact(4).zip(b.chunks_exact(4)) {
            let pair = (&left[..3], &right[..3]);
            if pair.0 == pair.1 { run = 0; previous = None; continue; }
            if previous == Some(pair) { run += 1; } else { run = 1; previous = Some(pair); }
            if run >= 40 { return true; }
        }
        false
    }).count()
}

#[test]
#[ignore = "Requires a local DWM session and ZSDESK_NATIVE_TEST_OUTPUT; creates only offscreen windows"]
fn full_window_keeps_selection_background_after_navigation() -> Result<()> {
    let output = PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("set ZSDESK_NATIVE_TEST_OUTPUT")?);
    ensure!(output.is_absolute(), "output must be an absolute path");
    fs::create_dir_all(&output)?;
    let first = output.join("fixture-first");
    let second = output.join("fixture-second");
    fixture(&first)?;
    fixture(&second)?;
    let foreground = unsafe { GetForegroundWindow() };
    unsafe { OleInitialize(None)?; }
    let apartment = Apartment;
    unsafe { InitCommonControlsEx(&INITCOMMONCONTROLSEX { dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
        dwICC: ICC_BAR_CLASSES | ICC_STANDARD_CLASSES }) }.ok()?;
    let instance = HINSTANCE(unsafe { GetModuleHandleW(None)? }.0);
    let class = wide("ZSDesk.Native.SelectionRegression");
    let wc = WNDCLASSW { lpfnWndProc: Some(window_proc), hInstance: instance,
        lpszClassName: pcwstr(&class), ..Default::default() };
    unsafe { RegisterClassW(&wc); }
    let mut session = ExplorerSession::default();
    session.tabs = vec![ExplorerTab::new(1, ShellLocation::Path(first)), ExplorerTab::new(2, ShellLocation::Path(second))];
    session.next_id = 3;
    let slot = Box::new(RefCell::new(Some(WindowState::new(session, output.join("profile/explorer-session.json"), None, true))));
    let right = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) + GetSystemMetrics(SM_CXVIRTUALSCREEN) };
    let bottom = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) + GetSystemMetrics(SM_CYVIRTUALSCREEN) };
    let owner = OwnedWindow(unsafe { CreateWindowExW(WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
        pcwstr(&class), windows::core::w!("Selection regression"), WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
        right + 1200, bottom + 1200, 1180, 760, None, None, Some(instance), Some((&*slot as *const Slot).cast()))? });
    unsafe { SetWindowPos(owner.0, None, 0, 0, 0, 0, SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE)?; }
    unsafe { slot.borrow_mut().as_mut().unwrap().create(owner.0)?; let _ = ShowWindow(owner.0, SW_SHOWNOACTIVATE); }
    let mut bounds = RECT::default();
    unsafe { GetWindowRect(owner.0, &mut bounds)?; }
    ensure!(bounds.left > right || bounds.top > bottom, "test window intersects the virtual screen");
    wait_navigation(&slot)?;
    let shell = view(&slot)?;
    let folder: IFolderView2 = shell.cast()?;
    unsafe { folder.SetViewModeAndIconSize(FVM_DETAILS, 16)?; }
    clear_selection(&shell)?;
    let (baseline, empty) = capture(&shell, &output, "unselected")?;
    let width = baseline["width"].as_u64().unwrap() as usize;
    select(&shell, 0, true)?;
    let (focused, focused_pixels) = capture(&shell, &output, "focused")?;
    let edit = slot.borrow().as_ref().unwrap().search_edit;
    unsafe { let _ = SetFocus(Some(edit)); }
    let (unfocused, unfocused_pixels) = capture(&shell, &output, "unfocused")?;
    select(&shell, 1, false)?;
    let (multiple, multiple_pixels) = capture(&shell, &output, "multiple")?;
    let selection_scan: Vec<Value> = (-1..=3).map(|start| {
        let mut index = -99;
        let status = unsafe { (folder.vtable().GetSelectedItem)(folder.as_raw(), start, &mut index) };
        json!({"start": start, "status": status.0, "index": index})
    }).collect();
    let item_sizes: Vec<Value> = (0..3).map(|index| {
        match unsafe { folder.Item(index) } {
            Ok(pointer) if !pointer.is_null() => {
                let bytes = unsafe { windows::Win32::UI::Shell::ILGetSize(Some(pointer)) };
                unsafe { windows::Win32::System::Com::CoTaskMemFree(Some(pointer.cast())); }
                json!({"index": index, "bytes": bytes})
            }
            other => json!({"index": index, "result": format!("{other:?}")}),
        }
    }).collect();
    drop(folder);
    drop(shell);
    unsafe { slot.borrow_mut().as_mut().unwrap().select_tab(1); }
    let stored_selected = slot.borrow().as_ref().unwrap().session.tabs[0].view.as_ref().map(|state| state.selected.len());
    wait_navigation(&slot)?;
    unsafe { slot.borrow_mut().as_mut().unwrap().select_tab(0); }
    wait_navigation(&slot)?;
    let restored_shell = view(&slot)?;
    let (restored, restored_pixels) = capture(&restored_shell, &output, "restored")?;
    select(&restored_shell, 2, true)?;
    let (reselected, reselected_pixels) = capture(&restored_shell, &output, "reselected")?;
    let message = MSG { hwnd: unsafe { GetFocus() }, message: WM_KEYDOWN,
        wParam: WPARAM(VK_A.0 as usize), ..Default::default() };
    let shortcut_view = slot.borrow().as_ref().unwrap().keyboard_view(&message, true).context("file shortcut focus route")?;
    let action = keyboard::resolve(VK_A.0, keyboard::Modifiers { control: true, ..Default::default() }, true, false).unwrap();
    keyboard::execute_message(&shortcut_view, owner.0, action, &message)?;
    drop(shortcut_view);
    let (all_selected, all_pixels) = capture(&restored_shell, &output, "select-all")?;
    clear_selection(&restored_shell)?;
    let (cleared, cleared_pixels) = capture(&restored_shell, &output, "cleared")?;
    drop(restored_shell);
    unsafe { slot.borrow_mut().as_mut().unwrap().select_tab(1); }
    wait_navigation(&slot)?;
    unsafe { slot.borrow_mut().as_mut().unwrap().select_tab(0); }
    wait_navigation(&slot)?;
    let empty_shell = view(&slot)?;
    let (restored_empty, restored_empty_pixels) = capture(&empty_shell, &output, "restored-empty")?;
    for frame in [&focused, &unfocused, &multiple, &restored, &reselected, &all_selected, &cleared, &restored_empty] {
        ensure!(frame["width"] == baseline["width"] && frame["height"] == baseline["height"], "capture dimensions changed");
    }
    let fills = json!({"focused": filled_rows(&empty, &focused_pixels, width),
        "unfocused": filled_rows(&empty, &unfocused_pixels, width),
        "multiple": filled_rows(&empty, &multiple_pixels, width),
        "restored": filled_rows(&empty, &restored_pixels, width),
        "reselected": filled_rows(&empty, &reselected_pixels, width),
        "select_all": filled_rows(&empty, &all_pixels, width)});
    let all_visible = fills.as_object().unwrap().values().all(|value| value.as_u64().unwrap_or(0) >= 6);
    let counts = stored_selected == Some(2) && focused["selected"] == 1 && unfocused["selected"] == 1 && multiple["selected"] == 2 && restored["selected"] == 2 && reselected["selected"] == 1 && all_selected["selected"] == 3 && cleared["selected"] == 0 && restored_empty["selected"] == 0;
    let single_rows = fills["focused"].as_u64().unwrap_or(0);
    let multiple_visible = ["multiple", "restored"].into_iter().all(|name|
        fills[name].as_u64().unwrap_or(0) >= single_rows.saturating_mul(2).saturating_sub(2))
        && fills["select_all"].as_u64().unwrap_or(0) >= single_rows.saturating_mul(3).saturating_sub(2);
    let cleared_rows = filled_rows(&empty, &cleared_pixels, width);
    let restored_empty_rows = filled_rows(&empty, &restored_empty_pixels, width);
    let report = json!({"state": if all_visible && counts && multiple_visible && cleared_rows < 6 && restored_empty_rows < 6 {"passed"} else {"failed"},
        "pid": std::process::id(), "shell_operations": 0, "undo_executed": false,
        "native_selection_scan": selection_scan, "item_pidl_sizes": item_sizes,
        "selection_counts_preserved": counts, "selection_backgrounds_visible": all_visible,
        "all_selected_rows_visible": multiple_visible,
        "owner_bounds": [bounds.left, bounds.top, bounds.right, bounds.bottom],
        "frames": [baseline, focused, unfocused, multiple, restored, reselected, all_selected, cleared, restored_empty], "filled_rows": fills,
        "stored_selected": stored_selected, "cleared_fill_rows": cleared_rows, "restored_empty_fill_rows": restored_empty_rows,
        "foreground_before": foreground.0 as usize,
        "foreground_before_cleanup": unsafe { GetForegroundWindow() }.0 as usize});
    drop(empty_shell);
    drop(owner);
    pump(80);
    drop(slot);
    drop(apartment);
    let mut report = report;
    report["foreground_after"] = json!(unsafe { GetForegroundWindow() }.0 as usize);
    report["foreground_unchanged"] = json!(foreground == unsafe { GetForegroundWindow() });
    if foreground != unsafe { GetForegroundWindow() } { report["state"] = json!("failed"); }
    fs::write(output.join("result.current.json"), serde_json::to_vec_pretty(&report)?)?;
    println!("{report}");
    ensure!(all_visible && multiple_visible, "selection background is not visible in all states");
    ensure!(counts, "selection counts were not preserved across navigation");
    ensure!(cleared_rows < 6 && restored_empty_rows < 6, "selection background remains after deselection");
    ensure!(foreground == unsafe { GetForegroundWindow() }, "foreground changed during the regression");
    Ok(())
}
