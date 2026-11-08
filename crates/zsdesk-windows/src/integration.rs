//! Per-user file manager integration (规划 §6.1.8 系统打开接管与恢复).
//!
//! Only `HKCU\Software\Classes` is touched; `explorer.exe` is never patched or
//! injected. Before any write every affected value is recorded (data, type or
//! absence) in a backup file; a failed write rolls back immediately and
//! "恢复系统资源管理器" replays the backup. `Folder` (Control Panel and other
//! virtual folders) is left with Explorer; This PC has an explicit owned verb.

use std::{io, path::Path};

use serde::{Deserialize, Serialize};
use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::ERROR_SUCCESS,
        System::Registry::{RegCloseKey, RegDeleteKeyValueW, RegDeleteTreeW, RegOpenKeyExW, RegQueryValueExW, RegSetKeyValueW,
            HKEY, HKEY_CURRENT_USER, KEY_READ, REG_SZ, REG_VALUE_TYPE},
    },
};

const CLASSES: &str = r"Software\Classes";
const VERB: &str = "ZSDesk";
/// Classes receiving the "使用 ZSDesk 打开" verb.
const THIS_PC_CLASS:&str=r"CLSID\{20D04FE0-3AEA-1069-A2D8-08002B30309D}";
const THIS_PC_ROOT:&str=r"Software\Classes\CLSID\{20D04FE0-3AEA-1069-A2D8-08002B30309D}";
const VERB_CLASSES: [&str; 4] = ["Directory", "Drive", r"Directory\Background",THIS_PC_CLASS];
/// Classes whose default verb becomes ZSDesk in default mode.
const DEFAULT_CLASSES: [&str; 3] = ["Directory", "Drive",THIS_PC_CLASS];
const WIN_E_ROOT: &str = r"Software\Classes\CLSID\{52205fd8-5dfb-447d-801a-d0b52f2e83e1}";
const WIN_E_COMMAND: &str = r"Software\Classes\CLSID\{52205fd8-5dfb-447d-801a-d0b52f2e83e1}\shell\opennewwindow\command";

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
    read_checked(key,name).unwrap_or(SavedValue{key:key.into(),name:name.map(String::from),kind:None,data:None})
}

fn read_checked(key: &str, name: Option<&str>) -> io::Result<SavedValue> {
    let path = wide(key);
    let value_name = name.map(wide);
    let mut saved = SavedValue { key: key.into(), name: name.map(Into::into), kind: None, data: None };
    let mut handle = HKEY::default();
    let opened=unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, PCWSTR(path.as_ptr()), None, KEY_READ, &mut handle) };
    if opened.0==2{return Ok(saved);}
    if opened!=ERROR_SUCCESS{return Err(io::Error::from_raw_os_error(opened.0 as i32));}
    let result=(||{
        let mut kind = REG_VALUE_TYPE(0);
        let mut size = 0u32;
        let queried=unsafe { RegQueryValueExW(handle, name_ptr(&value_name), None, Some(&mut kind), None, Some(&mut size)) };
        if queried.0==2{return Ok(saved);}
        if queried!=ERROR_SUCCESS{return Err(io::Error::from_raw_os_error(queried.0 as i32));}
        if size>65536{return Err(io::Error::new(io::ErrorKind::InvalidData,"系统集成值过大"));}
        let mut data = vec![0u8; size as usize];
        let queried=unsafe { RegQueryValueExW(handle, name_ptr(&value_name), None, Some(&mut kind), Some(data.as_mut_ptr()), Some(&mut size)) };
        if queried!=ERROR_SUCCESS{return Err(io::Error::from_raw_os_error(queried.0 as i32));}
        data.truncate(size as usize);saved.kind=Some(kind.0);saved.data=Some(data);
        Ok(saved)
    })();
    let _ = unsafe { RegCloseKey(handle) };
    result
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

fn notify() {
    #[cfg(not(test))] {
        use windows::Win32::UI::Shell::{SHChangeNotify,SHCNE_ASSOCCHANGED,SHCNF_IDLIST};
        unsafe {SHChangeNotify(SHCNE_ASSOCCHANGED,SHCNF_IDLIST,None,None)};
    }
}

fn win_e_command(exe: &Path) -> String { format!("\"{}\" --win-e", exe.display()) }

fn win_e_keys() -> Vec<String> {
    vec![WIN_E_ROOT.into(), format!(r"{WIN_E_ROOT}\shell"),
        format!(r"{WIN_E_ROOT}\shell\opennewwindow"), WIN_E_COMMAND.into()]
}

fn win_e_owned(exe: &Path) -> bool {
    read_string(WIN_E_COMMAND,None).is_some_and(|value| value.eq_ignore_ascii_case(&win_e_command(exe)))
        && read_string(WIN_E_COMMAND,Some("DelegateExecute")).as_deref()==Some("")
}
pub fn win_e_enabled(exe:&Path)->bool {win_e_owned(exe)}
pub fn this_pc_enabled(exe:&Path)->bool{
    read_string(&format!(r"{THIS_PC_ROOT}\shell"),None).as_deref()==Some(VERB)
        &&read_string(&format!(r"{THIS_PC_ROOT}\shell\{VERB}\command"),None).is_some_and(|value|command_points_to(&value,exe))
}

fn remove_created_key(key: &str) {
    if !key.starts_with(WIN_E_ROOT)&&!key.starts_with(THIS_PC_ROOT) { delete_tree(key); return; }
    // The Explorer launch class is shared. Remove only empty keys we created,
    // keeping values subsequently added by another application or the user.
    let path=wide(key);let mut handle=HKEY::default();
    if unsafe {RegOpenKeyExW(HKEY_CURRENT_USER,PCWSTR(path.as_ptr()),None,KEY_READ,&mut handle)}!=ERROR_SUCCESS{return;}
    let mut subkeys=0;let mut values=0;
    let status=unsafe {windows::Win32::System::Registry::RegQueryInfoKeyW(handle,None,None,None,Some(&mut subkeys),None,None,Some(&mut values),None,None,None,None)};
    let _=unsafe {RegCloseKey(handle)};
    if status==ERROR_SUCCESS && subkeys==0 && values==0 {let _=unsafe {windows::Win32::System::Registry::RegDeleteKeyW(HKEY_CURRENT_USER,PCWSTR(path.as_ptr()))};}
}

fn save_backup(path:&Path,backup:&Backup)->io::Result<()> {
    use std::io::Write;
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Storage::FileSystem::{MoveFileExW,MOVEFILE_REPLACE_EXISTING,MOVEFILE_WRITE_THROUGH};
    if let Some(parent)=path.parent(){std::fs::create_dir_all(parent)?;}
    let temporary=path.with_extension(format!("{}.pending",std::process::id()));
    let mut file=std::fs::File::create(&temporary)?;
    file.write_all(&serde_json::to_vec_pretty(backup).map_err(io::Error::other)?)?;file.sync_all()?;drop(file);
    let from:Vec<u16>=temporary.as_os_str().encode_wide().chain(Some(0)).collect();
    let to:Vec<u16>=path.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe {MoveFileExW(PCWSTR(from.as_ptr()),PCWSTR(to.as_ptr()),MOVEFILE_REPLACE_EXISTING|MOVEFILE_WRITE_THROUGH)}.map_err(|error|io::Error::other(error.to_string()))
}

fn validate_backup(backup:&Backup,exe:&Path)->io::Result<()> {
    let (allowed,keys)=plan(Integration::Default,exe);
    let mut seen=std::collections::HashSet::new();
    if backup.version!=1 || backup.values.len()>allowed.len() || backup.created_keys.len()>keys.len()
        || backup.values.iter().any(|value| value.kind.is_some()!=value.data.is_some()
            || value.data.as_ref().is_some_and(|bytes|bytes.len()>65536)
            || !seen.insert((&value.key,&value.name))
            || !allowed.iter().any(|(key,name,_)|key==&value.key && *name==value.name.as_deref()))
        || backup.created_keys.iter().any(|key|!keys.contains(key)) {
        return Err(io::Error::new(io::ErrorKind::InvalidData,"接管备份格式无效，未修改系统集成"));
    }
    Ok(())
}

/// Completes an existing owned default-manager registration without replaying
/// or replacing the user's original folder/drive restoration records.
pub fn upgrade_default(exe:&Path,backup_path:&Path)->io::Result<bool> {
    if current()!=Integration::Default || !registered_command().is_some_and(|command|command_points_to(&command,exe)) {return Ok(false);}
    let this_pc_command=format!(r"{CLASSES}\{THIS_PC_CLASS}\shell\{VERB}\command");
    let owns_this_pc=read_string(&this_pc_command,None).is_some_and(|value|command_points_to(&value,exe));
    if win_e_owned(exe)&&owns_this_pc{return Ok(false);}
    let bytes=std::fs::read(backup_path)?;
    let mut backup:Backup=serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    validate_backup(&backup,exe)?;
    // An owned record already exists but the command changed afterwards:
    // keep that later user choice; explicit reapplication can replace it.
    let (planned,keys)=plan(Integration::Default,exe);
    let planned:Vec<_>=planned.into_iter().filter(|(key,name,_)|(key==WIN_E_COMMAND||key.starts_with(&format!(r"{CLASSES}\{THIS_PC_CLASS}\shell")))
        && !backup.values.iter().any(|saved|saved.key==*key&&saved.name.as_deref()==*name)).collect();
    if planned.is_empty(){return Ok(false);}
    let before:Vec<_>=planned.iter().map(|(key,name,_)|read_checked(key,*name)).collect::<io::Result<_>>()?;
    backup.values.extend(before.iter().cloned());
    backup.created_keys.extend(keys.into_iter().filter(|key|!key_exists(key)&&!backup.created_keys.contains(key)).collect::<Vec<_>>());
    save_backup(backup_path,&backup)?;
    let result=planned.iter().try_for_each(|(key,name,value)|write_string(key,*name,value));
    if let Err(error)=result {
        for value in before.iter().rev(){restore_value(value)?;}
        for key in backup.created_keys.iter().rev().filter(|key|key.starts_with(WIN_E_ROOT)||key.starts_with(THIS_PC_ROOT)){remove_created_key(key);}
        // Keep the complete recovery record on every failure.
        notify();return Err(error);
    }
    if planned.iter().any(|(key,name,value)|read_string(key,*name).as_deref()!=Some(value.as_str())){return Err(io::Error::other("接管配置回读不一致，恢复记录已保留"));}
    notify();Ok(true)
}

/// Values to write for `level` (key, value name, data).
fn plan(level: Integration, exe: &Path) -> (Vec<(String, Option<&'static str>, String)>, Vec<String>) {
    let exe = exe.display().to_string();
    let mut values = Vec::new();
    let mut verb_keys = Vec::new();
    if level == Integration::Off { return (values, verb_keys); }
    verb_keys.extend([THIS_PC_ROOT.to_string(),format!(r"{THIS_PC_ROOT}\shell")]);
    for class in VERB_CLASSES {
        let verb = format!(r"{CLASSES}\{class}\shell\{VERB}");
        values.push((verb.clone(), None, "使用 ZSDesk 打开".to_string()));
        values.push((verb.clone(), Some("Icon"), format!("\"{exe}\",0")));
        values.push((format!(r"{verb}\command"), None, if class==THIS_PC_CLASS{format!("\"{exe}\" --this-pc")}else{format!("\"{exe}\" --shell-open \"%V\"" )}));
        verb_keys.push(verb);
        if class==THIS_PC_CLASS{verb_keys.push(format!(r"{THIS_PC_ROOT}\shell\{VERB}\command"));}
    }
    if level == Integration::Default {
        for class in DEFAULT_CLASSES { values.push((format!(r"{CLASSES}\{class}\shell"), None, VERB.to_string())); }
        values.push((WIN_E_COMMAND.into(),None,win_e_command(Path::new(&exe))));
        values.push((WIN_E_COMMAND.into(),Some("DelegateExecute"),String::new()));
        verb_keys.extend(win_e_keys());
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
    (arguments.trim_start().starts_with("--shell-open ")||arguments.trim()=="--this-pc")
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
    let own_win_e=win_e_owned(exe);
    let owned: Vec<_> = VERB_CLASSES.into_iter().filter(|class| {
        read_string(&format!(r"{CLASSES}\{class}\shell\{VERB}\command"), None)
            .is_some_and(|command| command_points_to(&command, exe))
    }).collect();
    if owned.is_empty() && !own_win_e { return Ok(false); }
    let backup: Option<Backup> = match std::fs::read(backup_path) {
        Ok(bytes) => Some(serde_json::from_slice(&bytes).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    if let Some(backup) = &backup {
        validate_backup(backup,exe)?;
    }
    if own_win_e {
        for name in [Some("DelegateExecute"),None] {
            let value=backup.as_ref().and_then(|backup|backup.values.iter().find(|value|value.key==WIN_E_COMMAND && value.name.as_deref()==name)).cloned()
                .unwrap_or(SavedValue{key:WIN_E_COMMAND.into(),name:name.map(String::from),kind:None,data:None});
            restore_value(&value)?;
        }
        if let Some(backup)=&backup {for key in backup.created_keys.iter().rev().filter(|key|key.starts_with(WIN_E_ROOT)){remove_created_key(key);}}
        else {for key in win_e_keys().iter().rev(){remove_created_key(key);}}
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
    if let Some(backup)=&backup{for key in backup.created_keys.iter().rev().filter(|key|key.starts_with(THIS_PC_ROOT)){remove_created_key(key);}}
    notify();
    Ok(true)
}

/// Switches to `level`, keeping a backup of everything it changes in `backup_path`.
pub fn apply(level: Integration, exe: &Path, backup_path: &Path) -> io::Result<()> {
    // Always start from the user's original configuration.
    if backup_path.try_exists()? {restore(backup_path)?;}
    else {restore_owned(exe,backup_path)?;}
    if level == Integration::Off { return Ok(()); }
    let (values, verb_keys) = plan(level, exe);
    let mut backup = Backup { version: 1, ..Default::default() };
    for (key, name, _) in &values { backup.values.push(read_checked(key, *name)?); }
    backup.created_keys = verb_keys.into_iter().filter(|k| !key_exists(k)).collect();
    save_backup(backup_path,&backup)?;
    for (key, name, data) in &values {
        if let Err(error) = write_string(key, *name, data) {
            // Transactional: undo everything written so far.
            if replay(&backup).is_ok() {let _ = std::fs::remove_file(backup_path);}
            notify();
            return Err(error);
        }
    }
    notify();
    Ok(())
}

fn replay(backup: &Backup) -> io::Result<()> {
    for value in backup.values.iter().rev() {
        restore_value(value)?;
    }
    for key in backup.created_keys.iter().rev() { remove_created_key(key); }
    Ok(())
}

/// Restores Explorer's folder opening from the backup ("恢复系统资源管理器").
pub fn restore(backup_path: &Path) -> io::Result<()> {
    match std::fs::read(backup_path) {
        Ok(bytes) => {
            let backup: Backup = serde_json::from_slice(&bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("接管备份已损坏，未修改注册表：{e}")))?;
            validate_backup(&backup,Path::new("ZSDesk.exe"))?;
            replay(&backup)?;
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
            if let Ok(exe)=std::env::current_exe(){if win_e_owned(&exe){
                delete_value(WIN_E_COMMAND,None);delete_value(WIN_E_COMMAND,Some("DelegateExecute"));
                for key in win_e_keys().iter().rev(){remove_created_key(key);}changed=true;
            }}
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
    fn malformed_or_unrelated_restore_records_are_rejected() {
        let invalid=Backup{version:1,values:vec![SavedValue{key:r"Software\OtherApp".into(),name:None,kind:None,data:None}],created_keys:vec![]};
        assert!(validate_backup(&invalid,Path::new("z.exe")).is_err());
        let (values,keys)=plan(Integration::ContextMenu,Path::new("z.exe"));
        assert!(values.iter().all(|(key,_,_)|!key.starts_with(WIN_E_ROOT)));
        assert!(keys.iter().all(|key|!key.starts_with(WIN_E_ROOT)));
    }

    #[test]
    #[ignore="Uses a process-local registry override and ZSDESK_NATIVE_TEST_OUTPUT; never dispatches the user's Win+E"]
    fn win_e_registration_upgrade_and_owned_restore()->Result<(),Box<dyn std::error::Error>> {
        use windows::Win32::System::Registry::{RegCreateKeyExW,RegOverridePredefKey,KEY_ALL_ACCESS,REG_OPTION_NON_VOLATILE,REG_EXPAND_SZ};
        use std::path::PathBuf;
        struct Sandbox{key:String,handle:HKEY,active:bool}
        impl Drop for Sandbox {fn drop(&mut self){
            if self.active {let _=unsafe{RegOverridePredefKey(HKEY_CURRENT_USER,None)};self.active=false;}
            let _=unsafe{RegCloseKey(self.handle)};delete_tree(&self.key);
        }}
        let output=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").ok_or("missing test output")?);
        assert!(output.is_absolute());std::fs::create_dir_all(&output)?;
        let key=format!(r"Software\ZSDesk.WinE.Regression.{}",std::process::id());
        assert!(!key_exists(&key),"test registry root already exists");
        let raw=wide(&key);let mut handle=HKEY::default();
        unsafe{RegCreateKeyExW(HKEY_CURRENT_USER,PCWSTR(raw.as_ptr()),None,PCWSTR::null(),REG_OPTION_NON_VOLATILE,KEY_ALL_ACCESS,None,&mut handle,None)}.ok()?;
        let mut sandbox=Sandbox{key,handle,active:false};
        unsafe{RegOverridePredefKey(HKEY_CURRENT_USER,Some(handle))}.ok()?;sandbox.active=true;
        let exe=Path::new(r"C:\Program Files\测试 ZSDesk\ZSDesk.exe");
        let backup_file=output.join("win-e-test-backup.json");
        assert!(!backup_file.exists(),"previous test backup requires inspection");
        let result=(||->Result<(),Box<dyn std::error::Error>>{
            write_string(r"Software\Classes\Directory\shell",None,"previous-manager")?;
            let original:Vec<u8>=wide(r"%SystemRoot%\explorer.exe").iter().flat_map(|unit|unit.to_le_bytes()).collect();
            write_raw(WIN_E_COMMAND,None,REG_EXPAND_SZ.0,&original)?;
            write_string(WIN_E_COMMAND,Some("DelegateExecute"),"{11dbb47c-a525-400b-9e80-a54615a090c0}")?;
            let old_command=read_checked(WIN_E_COMMAND,None)?;let old_delegate=read_checked(WIN_E_COMMAND,Some("DelegateExecute"))?;
            apply(Integration::Default,exe,&backup_file)?;
            assert!(win_e_enabled(exe));assert!(this_pc_enabled(exe));assert_eq!(current(),Integration::Default);
            assert!(restore_owned(exe,&backup_file)?);
            assert_eq!(read_checked(WIN_E_COMMAND,None)?,old_command);
            assert_eq!(read_checked(WIN_E_COMMAND,Some("DelegateExecute"))?,old_delegate);
            assert_eq!(read_string(r"Software\Classes\Directory\shell",None).as_deref(),Some("previous-manager"));
            assert!(!key_exists(THIS_PC_ROOT),"empty This PC override must be removed on restore");
            restore(&backup_file)?;
            delete_tree(WIN_E_ROOT);
            let (mut legacy_values,mut legacy_keys)=plan(Integration::Default,exe);
            legacy_values.retain(|(key,_,_)|!key.starts_with(WIN_E_ROOT)&&!key.starts_with(THIS_PC_ROOT));legacy_keys.retain(|key|!key.starts_with(WIN_E_ROOT)&&!key.starts_with(THIS_PC_ROOT));
            let mut legacy=Backup{version:1,..Default::default()};
            for (key,name,_) in &legacy_values{legacy.values.push(read_checked(key,*name)?);}
            legacy.created_keys=legacy_keys.into_iter().filter(|key|!key_exists(key)).collect();
            save_backup(&backup_file,&legacy)?;
            for (key,name,value) in &legacy_values{write_string(key,*name,value)?;}
            assert!(upgrade_default(exe,&backup_file)?);assert!(win_e_enabled(exe));assert!(this_pc_enabled(exe));
            let upgraded:Backup=serde_json::from_slice(&std::fs::read(&backup_file)?)?;
            assert_eq!(&upgraded.values[..legacy.values.len()],&legacy.values);
            assert!(!upgrade_default(exe,&backup_file)?);
            restore(&backup_file)?;
            assert!(!key_exists(WIN_E_ROOT),"empty user CLSID must not shadow the system class");
            apply(Integration::Default,exe,&backup_file)?;
            write_string(WIN_E_COMMAND,None,r#""C:\Other\Manager.exe""#)?;
            assert!(restore_owned(exe,&backup_file)?);
            assert_eq!(read_string(WIN_E_COMMAND,None).as_deref(),Some(r#""C:\Other\Manager.exe""#));
            restore(&backup_file)?;
            Ok(())
        })();
        unsafe{RegOverridePredefKey(HKEY_CURRENT_USER,None)}.ok()?;sandbox.active=false;
        let registry_key=sandbox.key.clone();drop(sandbox);
        let cleaned=!key_exists(&registry_key);
        let report=serde_json::json!({"state":if result.is_ok()&&cleaned{"passed"}else{"failed"},"pid":std::process::id(),
            "registry_override_process_only":true,"registry_cleanup":cleaned,"original_value_type_restored":result.is_ok(),
            "legacy_backup_preserved":result.is_ok(),"foreign_command_preserved":result.is_ok(),"keyboard_input_sent":false,
            "error":result.as_ref().err().map(|error|error.to_string())});
        std::fs::write(output.join("win-e.current.json"),serde_json::to_vec_pretty(&report)?)?;println!("{report}");
        result?;assert!(cleaned);Ok(())
    }
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
        assert_eq!(keys.len(), 11);
        assert!(values.iter().any(|(key,name,value)|key==&format!(r"{THIS_PC_ROOT}\shell")&&name.is_none()&&value==VERB));
        assert!(values.iter().any(|(key,name,value)|key==&format!(r"{THIS_PC_ROOT}\shell\{VERB}\command")&&name.is_none()&&value==r#""C:\Apps\ZSDesk.exe" --this-pc"#));
        assert!(values.iter().any(|(key,name,value)|key==WIN_E_COMMAND && name.is_none() && value==r#""C:\Apps\ZSDesk.exe" --win-e"#));
        assert!(values.iter().any(|(key,name,value)|key==WIN_E_COMMAND && *name==Some("DelegateExecute") && value.is_empty()));
        let (context, _) = plan(Integration::ContextMenu, Path::new("z.exe"));
        assert!(!context.iter().any(|(k, n, _)| k.ends_with(r"\shell") && n.is_none()), "context mode keeps Explorer as default");
        assert!(plan(Integration::Off, Path::new("z.exe")).0.is_empty());
    }
}
