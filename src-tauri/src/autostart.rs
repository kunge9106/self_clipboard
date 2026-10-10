//! 开机自启：写 HKCU\...\CurrentVersion\Run 注册表键。
//!
//! 手写 advapi32 FFI，与项目「不为几个 API 拖入庞大依赖」的原则一致。
//! 值数据为带引号的 exe 全路径，避免带空格路径被拆参。

use std::fs;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE_NAME: &str = "轻贴";

const HKEY_CURRENT_USER: isize = 0x8000_0001u32 as isize;
const KEY_READ: u32 = 0x20019;
const KEY_WRITE: u32 = 0x20006;
const REG_SZ: u32 = 1;
const ERROR_SUCCESS: i32 = 0;

#[link(name = "advapi32")]
extern "system" {
    fn RegCreateKeyExW(
        hkey: isize,
        subkey: *const u16,
        reserved: u32,
        class: *const u16,
        options: u32,
        sam_desired: u32,
        security: *mut u8,
        result: *mut isize,
        disposition: *mut u32,
    ) -> i32;
    fn RegOpenKeyExW(hkey: isize, subkey: *const u16, options: u32, sam_desired: u32, result: *mut isize) -> i32;
    fn RegQueryValueExW(
        hkey: isize,
        name: *const u16,
        reserved: *mut u32,
        typ: *mut u32,
        data: *mut u8,
        cb: *mut u32,
    ) -> i32;
    fn RegSetValueExW(
        hkey: isize,
        name: *const u16,
        reserved: u32,
        typ: u32,
        data: *const u8,
        cb: u32,
    ) -> i32;
    fn RegDeleteValueW(hkey: isize, name: *const u16) -> i32;
    fn RegCloseKey(hkey: isize) -> i32;
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 是否已存在自启项（不校验路径内容，存在即视为开启）
pub fn is_enabled() -> bool {
    unsafe {
        let mut hkey: isize = 0;
        if RegOpenKeyExW(HKEY_CURRENT_USER, wide(RUN_KEY).as_ptr(), 0, KEY_READ, &mut hkey)
            != ERROR_SUCCESS
        {
            return false;
        }
        let mut typ: u32 = 0;
        let mut cb: u32 = 0;
        let r = RegQueryValueExW(hkey, wide(VALUE_NAME).as_ptr(), std::ptr::null_mut(), &mut typ, std::ptr::null_mut(), &mut cb);
        RegCloseKey(hkey);
        r == ERROR_SUCCESS
    }
}

pub fn set_enabled(enable: bool) -> Result<(), String> {
    if enable {
        let exe = std::env::current_exe()
            .map_err(|e| format!("取 exe 路径失败：{e}"))?
            .to_string_lossy()
            .to_string();
        let _ = fs::canonicalize(&exe); // 不强制成功：路径规范化失败就用原样
        let data = format!("\"{}\"", exe);
        let bytes = data.as_bytes();

        unsafe {
            let mut hkey: isize = 0;
            let mut disp: u32 = 0;
            if RegCreateKeyExW(
                HKEY_CURRENT_USER,
                wide(RUN_KEY).as_ptr(),
                0,
                std::ptr::null(),
                0,
                KEY_WRITE,
                std::ptr::null_mut(),
                &mut hkey,
                &mut disp,
            ) != ERROR_SUCCESS
            {
                return Err("打开注册表 Run 键失败".into());
            }
            let r = RegSetValueExW(
                hkey,
                wide(VALUE_NAME).as_ptr(),
                0,
                REG_SZ,
                bytes.as_ptr(),
                bytes.len() as u32,
            );
            RegCloseKey(hkey);
            if r != ERROR_SUCCESS {
                return Err(format!("写注册表失败（code {r}）"));
            }
        }
        println!("[autostart] 已开启开机自启：{data}");
    } else {
        unsafe {
            let mut hkey: isize = 0;
            if RegOpenKeyExW(HKEY_CURRENT_USER, wide(RUN_KEY).as_ptr(), 0, KEY_WRITE, &mut hkey)
                != ERROR_SUCCESS
            {
                // 键不存在 = 本来就没有自启项
                return Ok(());
            }
            let r = RegDeleteValueW(hkey, wide(VALUE_NAME).as_ptr());
            RegCloseKey(hkey);
            // 值不存在也视为成功
            if r != ERROR_SUCCESS && r != 2 {
                return Err(format!("删除注册表值失败（code {r}）"));
            }
        }
        println!("[autostart] 已关闭开机自启");
    }
    Ok(())
}
