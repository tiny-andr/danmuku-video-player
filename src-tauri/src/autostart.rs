// 开机自启动：写当前用户的 Run 键（HKCU\Software\Microsoft\Windows\CurrentVersion\Run）。
// 用注册表而不是任务计划，删掉这个值就彻底停用，不需要额外组件。
// 值名固定英文，避免中文键名在某些系统上编码异常；路径带空格所以统一加引号。

#![cfg(windows)]

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;

use windows_sys::Win32::System::Registry::{
    RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY, HKEY_CURRENT_USER, REG_SZ,
    RRF_RT_REG_SZ,
};

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE_NAME: &str = "Danmuku Video Player";

fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

fn exists() -> bool {
    let key = wide(RUN_KEY);
    let name = wide(VALUE_NAME);
    let mut buf = [0u16; 1024];
    let mut size = std::mem::size_of_val(&buf) as u32;
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER as HKEY,
            key.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buf.as_mut_ptr() as *mut core::ffi::c_void,
            &mut size,
        )
    };
    rc == 0
}

fn set(enable: bool) -> Result<bool, String> {
    let key = wide(RUN_KEY);
    let name = wide(VALUE_NAME);
    if enable {
        let exe = std::env::current_exe().map_err(|e| format!("取程序路径失败: {e}"))?;
        let cmd = format!("\"{}\"", exe.to_string_lossy());
        let data = wide(&cmd);
        let bytes = (data.len() * 2) as u32;
        let rc = unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER as HKEY,
                key.as_ptr(),
                name.as_ptr(),
                REG_SZ,
                data.as_ptr() as *const core::ffi::c_void,
                bytes,
            )
        };
        if rc != 0 {
            return Err(format!("写注册表失败（错误码 {rc}）"));
        }
    } else {
        let rc = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER as HKEY, key.as_ptr(), name.as_ptr()) };
        // 值本来就不存在也算成功
        if rc != 0 && rc != 2 {
            return Err(format!("删注册表项失败（错误码 {rc}）"));
        }
    }
    Ok(exists())
}

#[tauri::command]
pub fn get_autostart() -> bool {
    exists()
}

#[tauri::command]
pub fn set_autostart(enable: bool) -> Result<bool, String> {
    set(enable)
}
