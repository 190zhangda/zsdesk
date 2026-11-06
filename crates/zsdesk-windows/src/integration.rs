//! Per-user file manager integration (规划 §6.1.8 系统打开接管与恢复).
//!
//! Only `HKCU\Software\Classes` is touched; `explorer.exe` is never patched or
//! injected. Before any write every affected value is recorded (data, type or
//! absence) in a backup file; a failed write rolls back immediately and
//! "恢复系统资源管理器" replays the backup. `Folder` (Control Panel and other
//! virtual folders) is deliberately left with Explorer.

use std::{io, path::Path};

use serde::{Deserialize, Serialize};
use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::ERROR_SUCCESS,
        System::Registry::{RegCloseKey, RegDeleteKeyValueW, RegDeleteTreeW, RegOpenKeyExW, RegQueryValueExW, RegSetKeyValueW,
            HKEY, HKEY_CURRENT_USER, KEY_READ, REG_SZ, REG_VALUE_TYPE},
        UI::Shell::{SHChangeNotify, SHCNE_ASSOCCHANGED, SHCNF_IDLIST},
    },
};

const CLASSES: &str = r"Software\Classes";
const VERB: &str = "ZSDesk";
/// Classes receiving the "使用 ZSDesk 打开" verb.
const VERB_CLASSES: [&str; 3] = ["Directory", "Drive", r"Directory\Background"];
/// Classes whose default verb becomes ZSDesk in default mode.
const DEFAULT_CLASSES: [&str; 2] = ["Directory", "Drive"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Integration {
    /// ZSDesk does not change folder opening.
    Off,
    /// Adds "使用 ZSDesk 打开" next to Explorer's own verbs.
    ContextMenu,
    /// Folders and drives open in ZSDesk by default.
    Default,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct SavedValue { key: String, name: Option<String>, kind: Option<u32>, data: Option<Vec<u8>> }

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Backup { version: u32, values: Vec<SavedValue>, created_keys: Vec<String> }

fn wide(text: &str) -> Vec<u16> { text.encode_utf16().chain(Some(0)).collect() }
fn name_ptr(name: &Option<Vec<u16>>) -> PCWSTR { name.as_ref().map_or(PCWSTR::null(), |n| PCWSTR(n.as_ptr())) }

fn key_exists(key: &str) -> bool {
    let path = wide(key);
    let mut handle = HKEY::default();
    let ok = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, PCWSTR(path.as_ptr()), None, KEY_READ, &mut handle) } == ERROR_SUCCESS;
    if ok { let _ = unsafe { RegCloseKey(handle) }; }
    ok
}

fn read(key: &str, name: Option<&str>) -> SavedValue {
    let path = wide(key);
    let value_name = name.map(wide);
    let mut saved = SavedValue { key: key.into(), name: name.map(Into::into), kind: None, data: None };
    let mut handle = HKEY::default();
    if unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, PCWSTR(path.as_ptr()), None, KEY_READ, &mut handle) } != ERROR_SUCCESS { return saved; }
    let mut kind = REG_VALUE_TYPE(0);
    let mut size = 0u32;
    if unsafe { RegQueryValueExW(handle, name_ptr(&value_name), None, Some(&mut kind), None, Some(&mut size)) } == ERROR_SUCCESS {
        let mut data = vec![0u8; size as usize];
        if unsafe { RegQueryValueExW(handle, name_ptr(&value_name), None, Some(&mut kind), Some(data.as_mut_ptr()), Some(&mut size)) } == ERROR_SUCCESS {
            data.truncate(size as usize);
            saved.kind = Some(kind.0);
            saved.data = Some(data);
        }
    }
    let _ = unsafe { RegCloseKey(handle) };
    saved
}

fn read_string(key: &str, name: Option<&str>) -> Option<String> {
    let value = read(key, name);
    let data = value.data?;
    let units: Vec<u16> = data.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&c| c != 0).collect();
    Some(String::from_utf16_lossy(&units))
}

fn write_raw(key: &str, name: Option<&str>, kind: u32, data: &[u8]) -> io::Result<()> {
    let path = wide(key);
    let value_name = name.map(wide);
    let status = unsafe { RegSetKeyValueW(HKEY_CURRENT_USER, PCWSTR(path.as_ptr()), name_ptr(&value_name), kind, Some(data.as_ptr().cast()), data.len() as u32) };
    if status == ERROR_SUCCESS { Ok(()) } else { Err(io::Error::from_raw_os_error(status.0 as i32)) }
}

fn write_string(key: &str, name: Option<&str>, value: &str) -> io::Result<()> {
    let bytes: Vec<u8> = wide(value).iter().flat_map(|u| u.to_le_bytes()).collect();
    write_raw(key, name, REG_SZ.0, &bytes)
}

fn delete_value(key: &str, name: Option<&str>) {
    let path = wide(key);
    let value_name = name.map(wide);
    let _ = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, PCWSTR(path.as_ptr()), name_ptr(&value_name)) };
}

fn delete_tree(key: &str) {
    let path = wide(key);
    let _ = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(path.as_ptr())) };
    // RegDeleteTreeW empties the key; remove the key itself as well.
    let _ = unsafe { windows::Win32::System::Registry::RegDeleteKeyW(HKEY_CURRENT_USER, PCWSTR(path.as_ptr())) };
}

fn notify() { unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None) }; }

/// Values to write for `level` (key, value name, data).
fn plan(level: Integration, exe: &Path) -> (Vec<(String, Option<&'static str>, String)>, Vec<String>) {
    let exe = exe.display().to_string();
    let mut values = Vec::new();
    let mut verb_keys = Vec::new();
    if level == Integration::Off { return (values, verb_keys); }
    for class in VERB_CLASSES {
        let verb = format!(r"{CLASSES}\{class}\shell\{VERB}");
        values.push((verb.clone(), None, "使用 ZSDesk 打开".to_string()));
        values.push((verb.clone(), Some("Icon"), format!("\"{exe}\",0")));
        values.push((format!(r"{verb}\command"), None, format!("\"{exe}\" --shell-open \"%V\"")));
        verb_keys.push(verb);
    }
    if level == Integration::Default {
        for class in DEFAULT_CLASSES { values.push((format!(r"{CLASSES}\{class}\shell"), None, VERB.to_string())); }
    }
    (values, verb_keys)
}

/// Current integration level registered for this user.
pub fn current() -> Integration {
    let default = read_string(&format!(r"{CLASSES}\Directory\shell"), None);
    if default.as_deref() == Some(VERB) { return Integration::Default; }
    if key_exists(&format!(r"{CLASSES}\Directory\shell\{VERB}\command")) { return Integration::ContextMenu; }
    Integration::Off
}

/// Executable referenced by the registered verb, if any (health check after moves/updates).
pub fn registered_command() -> Option<String> { read_string(&format!(r"{CLASSES}\Directory\shell\{VERB}\command"), None) }

fn command_points_to(command: &str, exe: &Path) -> bool {
    let Some((path, arguments)) = command.trim().strip_prefix('"').and_then(|text| text.split_once('"')) else { return false };
    arguments.trim_start().starts_with("--shell-open ")
        && path.replace('/', "\\").eq_ignore_ascii_case(&exe.to_string_lossy().replace('/', "\\"))
}

fn restore_value(value: &SavedValue) -> io::Result<()> {
    if let (Some(kind), Some(data)) = (value.kind, &value.data) { return write_raw(&value.key, value.name.as_deref(), kind, data); }
    let path = wide(&value.key);
    let name = value.name.as_deref().map(wide);
    let status = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, PCWSTR(path.as_ptr()), name_ptr(&name)) };
    if status == ERROR_SUCCESS || status.0 == 2 { Ok(()) } else { Err(io::Error::from_raw_os_error(status.0 as i32)) }
}

/// Uninstaller cleanup is limited to verbs still owned by this executable.
/// Later choices made by the user or another installation remain in place.
/// The profile and its original restoration record are retained.
pub fn restore_owned(exe: &Path, backup_path: &Path) -> io::Result<bool> {
    let owned: Vec<_> = VERB_CLASSES.into_iter().filter(|class| {
        read_string(&format!(r"{CLASSES}\{class}\shell\{VERB}\command"), None)
            .is_some_and(|command| command_points_to(&command, exe))
    }).collect();
    if owned.is_empty() { return Ok(false); }
    let backup: Option<Backup> = match std::fs::read(backup_path) {
        Ok(bytes) => Some(serde_json::from_slice(&bytes).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    if let Some(backup) = &backup {
        let (allowed, keys) = plan(Integration::Default, exe);
        if backup.version != 1 || backup.values.iter().any(|value|
            value.kind.is_some() != value.data.is_some() || !allowed.iter().any(|(key, name, _)| key == &value.key && *name == value.name.as_deref()))
            || backup.created_keys.iter().any(|key| !keys.contains(key)) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "接管备份格式无效，未修改系统集成"));
        }
    }
    for class in owned {
        let shell = format!(r"{CLASSES}\{class}\shell");
        let verb = format!(r"{shell}\{VERB}");
        if DEFAULT_CLASSES.contains(&class) && read_string(&shell, None).is_some_and(|value| value.eq_ignore_ascii_case(VERB)) {
            let value = backup.as_ref().and_then(|backup| backup.values.iter().find(|value| value.key == shell && value.name.is_none())).cloned()
                .unwrap_or(SavedValue { key: shell, name: None, kind: None, data: None });
            restore_value(&value)?;
        }
        if backup.as_ref().is_none_or(|backup| backup.created_keys.contains(&verb)) {
            delete_tree(&verb);
            if key_exists(&verb) { return Err(io::Error::other("无法移除当前程序的系统集成项")); }
        } else if let Some(backup) = &backup {
            let command = format!(r"{verb}\command");
            for value in backup.values.iter().rev().filter(|value| value.key == verb || value.key == command) { restore_value(value)?; }
        }
    }
    notify();
    Ok(true)
}

/// Switches to `level`, keeping a backup of everything it changes in `backup_path`.
pub fn apply(level: Integration, exe: &Path, backup_path: &Path) -> io::Result<()> {
    // Always start from the user's original configuration.
    restore(backup_path)?;
    if level == Integration::Off { return Ok(()); }
    let (values, verb_keys) = plan(level, exe);
    let mut backup = Backup { version: 1, ..Default::default() };
    for (key, name, _) in &values { backup.values.push(read(key, *name)); }
    backup.created_keys = verb_keys.into_iter().filter(|k| !key_exists(k)).collect();
    if let Some(parent) = backup_path.parent() { std::fs::create_dir_all(parent)?; }
    std::fs::write(backup_path, serde_json::to_vec_pretty(&backup).map_err(io::Error::other)?)?;
    for (key, name, data) in &values {
        if let Err(error) = write_string(key, *name, data) {
            // Transactional: undo everything written so far.
            replay(&backup);
            let _ = std::fs::remove_file(backup_path);
            notify();
            return Err(error);
        }
    }
    notify();
    Ok(())
}

fn replay(backup: &Backup) {
    for value in backup.values.iter().rev() {
        match (value.kind, &value.data) {
            (Some(kind), Some(data)) => { let _ = write_raw(&value.key, value.name.as_deref(), kind, data); }
            _ => delete_value(&value.key, value.name.as_deref()),
        }
    }
    for key in &backup.created_keys { delete_tree(key); }
}

/// Restores Explorer's folder opening from the backup ("恢复系统资源管理器").
pub fn restore(backup_path: &Path) -> io::Result<()> {
    match std::fs::read(backup_path) {
        Ok(bytes) => {
            let backup: Backup = serde_json::from_slice(&bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("接管备份已损坏，未修改注册表：{e}")))?;
            replay(&backup);
            std::fs::remove_file(backup_path)?;
            notify();
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            // No backup: only remove what ZSDesk itself would have written.
            let mut changed = false;
            for class in DEFAULT_CLASSES {
                let key = format!(r"{CLASSES}\{class}\shell");
                if read_string(&key, None).as_deref() == Some(VERB) { delete_value(&key, None); changed = true; }
            }
            for class in VERB_CLASSES {
                let key = format!(r"{CLASSES}\{class}\shell\{VERB}");
                if key_exists(&key) { delete_tree(&key); changed = true; }
            }
            if changed { notify(); }
        }
        Err(error) => return Err(error),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn uninstaller_only_recognizes_the_registered_executable_and_verb() {
        let exe = Path::new(r"C:\Apps\ZSDesk\ZSDesk.exe");
        assert!(command_points_to(r#""c:\apps\zsdesk\zsdesk.exe" --shell-open "%V""#, exe));
        assert!(!command_points_to(r#""C:\Other\ZSDesk.exe" --shell-open "%V""#, exe));
        assert!(!command_points_to(r#""C:\Apps\ZSDesk\ZSDesk.exe" --desktop"#, exe));
        assert!(!command_points_to(r#"cmd /c "C:\Apps\ZSDesk\ZSDesk.exe" --shell-open "%V""#, exe));
    }
    #[test]
    fn plans_only_touch_per_user_classes() {
        let (values, keys) = plan(Integration::Default, Path::new(r"C:\Apps\ZSDesk.exe"));
        assert!(values.iter().all(|(k, _, _)| k.starts_with(r"Software\Classes\")));
        assert!(values.iter().any(|(k, n, v)| k.ends_with(r"Directory\shell") && n.is_none() && v == "ZSDesk"));
        assert!(!values.iter().any(|(k, _, _)| k.contains(r"\Folder\")), "virtual folders stay with Explorer");
        assert!(values.iter().any(|(_, _, v)| v == r#""C:\Apps\ZSDesk.exe" --shell-open "%V""#));
        assert_eq!(keys.len(), 3);
        let (context, _) = plan(Integration::ContextMenu, Path::new("z.exe"));
        assert!(!context.iter().any(|(k, n, _)| k.ends_with(r"\shell") && n.is_none()), "context mode keeps Explorer as default");
        assert!(plan(Integration::Off, Path::new("z.exe")).0.is_empty());
    }
}
