//! System theme detection and DWM frame attributes.
//!
//! Dark mode for Shell-hosted controls uses the uxtheme ordinals
//! `SetPreferredAppMode` (#135), `AllowDarkModeForWindow` (#133) and
//! `FlushMenuThemes` (#136). They are only resolved on Windows 10 1903+
//! (build 18362) where their signatures are stable; otherwise light controls
//! remain and only the ZSDesk chrome follows the dark palette.

use windows::{
    core::{w, PCSTR, PCWSTR},
    Win32::{
        Foundation::{HMODULE, HWND, RECT},
        Graphics::Dwm::{DwmExtendFrameIntoClientArea, DwmGetWindowAttribute, DwmSetWindowAttribute,
            DWMWA_CAPTION_BUTTON_BOUNDS, DWMWA_SYSTEMBACKDROP_TYPE, DWMWA_USE_IMMERSIVE_DARK_MODE, DWMSBT_MAINWINDOW},
        System::{LibraryLoader::{GetProcAddress, LoadLibraryW}, Registry::{RegGetValueW, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ}},
        UI::{Controls::{SetWindowTheme, MARGINS}},
    },
};

/// Windows build number (e.g. 22631), read from the registry because
/// `GetVersionEx` is manifest-dependent.
pub fn os_build() -> u32 {
    static BUILD: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *BUILD.get_or_init(|| {
        let mut buffer = [0u16; 32];
        let mut size = (buffer.len() * 2) as u32;
        let ok = unsafe { RegGetValueW(HKEY_LOCAL_MACHINE, w!("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion"),
            w!("CurrentBuildNumber"), RRF_RT_REG_SZ, None, Some(buffer.as_mut_ptr().cast()), Some(&mut size)) }.is_ok();
        if !ok { return 0; }
        let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
        String::from_utf16_lossy(&buffer[..end]).trim().parse().unwrap_or(0)
    })
}

/// True when "Choose your app mode" is Dark.
pub fn system_dark_mode() -> bool {
    let mut value = 1u32;
    let mut size = 4u32;
    let ok = unsafe { RegGetValueW(HKEY_CURRENT_USER, w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
        w!("AppsUseLightTheme"), RRF_RT_REG_DWORD, None, Some((&mut value as *mut u32).cast()), Some(&mut size)) }.is_ok();
    ok && value == 0
}

type SetPreferredAppMode = unsafe extern "system" fn(i32) -> i32;
type AllowDarkModeForWindow = unsafe extern "system" fn(HWND, i32) -> i32;
type FlushMenuThemes = unsafe extern "system" fn();

fn uxtheme_ordinal(ordinal: usize) -> Option<unsafe extern "system" fn() -> isize> {
    if os_build() < 18362 { return None; }
    // Keep one process-lifetime reference; navigation reapplies child themes.
    static MODULE: std::sync::OnceLock<Option<usize>> = std::sync::OnceLock::new();
    let address = (*MODULE.get_or_init(|| unsafe { LoadLibraryW(w!("uxtheme.dll")).ok().map(|m| m.0 as usize) }))?;
    let module = HMODULE(address as *mut _);
    unsafe { GetProcAddress(module, PCSTR(ordinal as *const u8)) }
}

/// Lets Shell views, trees and menus in this process use dark visuals.
pub fn allow_dark_mode(dark: bool) {
    if let Some(proc) = uxtheme_ordinal(135) {
        let set: SetPreferredAppMode = unsafe { std::mem::transmute(proc) };
        unsafe { set(if dark { 1 } else { 0 }); }
    }
    if let Some(proc) = uxtheme_ordinal(136) {
        let flush: FlushMenuThemes = unsafe { std::mem::transmute(proc) };
        unsafe { flush(); }
    }
}

/// Applies dark/light control theme to a hosted window (tree, list).
pub fn theme_control(hwnd: HWND, dark: bool) {
    if hwnd.0.is_null() { return; }
    if let Some(proc) = uxtheme_ordinal(133) {
        let allow: AllowDarkModeForWindow = unsafe { std::mem::transmute(proc) };
        unsafe { allow(hwnd, dark as i32); }
    }
    let name = if dark { w!("DarkMode_Explorer") } else { w!("Explorer") };
    let _ = unsafe { SetWindowTheme(hwnd, name, PCWSTR::null()) };
}

/// Dark title bar / caption buttons (attribute 20, or 19 before build 18985).
pub fn set_dark_frame(hwnd: HWND, dark: bool) {
    let value: i32 = dark as i32;
    let attribute = if os_build() >= 18985 { DWMWA_USE_IMMERSIVE_DARK_MODE } else { windows::Win32::Graphics::Dwm::DWMWINDOWATTRIBUTE(19) };
    let _ = unsafe { DwmSetWindowAttribute(hwnd, attribute, (&value as *const i32).cast(), 4) };
}

/// Windows 11 rounded window corners (`small` for popups such as tooltips and editors).
/// Earlier systems keep square corners; ZSDesk draws its own rounded surfaces there.
pub fn round_corners(hwnd: HWND, small: bool) {
    if os_build() < 22000 || hwnd.0.is_null() { return; }
    let value: i32 = if small { 3 } else { 2 }; // DWMWCP_ROUNDSMALL / DWMWCP_ROUND
    let _ = unsafe { DwmSetWindowAttribute(hwnd, windows::Win32::Graphics::Dwm::DWMWA_WINDOW_CORNER_PREFERENCE, (&value as *const i32).cast(), 4) };
}

/// Enables the Mica backdrop on Windows 11 22H2+. Returns whether it applies.
pub fn enable_mica(hwnd: HWND) -> bool {
    if os_build() < 22621 { return false; }
    let value = DWMSBT_MAINWINDOW.0;
    unsafe { DwmSetWindowAttribute(hwnd, DWMWA_SYSTEMBACKDROP_TYPE, (&value as *const i32).cast(), 4) }.is_ok()
}

/// Extends the DWM frame `top` pixels into the client area (caption buttons and backdrop).
pub fn extend_frame(hwnd: HWND, top: i32) {
    let margins = MARGINS { cxLeftWidth: 0, cxRightWidth: 0, cyTopHeight: top, cyBottomHeight: 0 };
    let _ = unsafe { DwmExtendFrameIntoClientArea(hwnd, &margins) };
}

/// Caption button bounds in window coordinates.
pub fn caption_button_bounds(hwnd: HWND) -> Option<RECT> {
    let mut rect = RECT::default();
    unsafe { DwmGetWindowAttribute(hwnd, DWMWA_CAPTION_BUTTON_BOUNDS, (&mut rect as *mut RECT).cast(), std::mem::size_of::<RECT>() as u32) }.ok()?;
    (rect.right > rect.left).then_some(rect)
}
