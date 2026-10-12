//! 开机自启：写 HKCU\...\CurrentVersion\Run 注册表键。
//!
//! 手写 advapi32 FFI，与项目「不为几个 API 拖入庞大依赖」的原则一致。
//! 值数据为带引号的 exe 全路径，避免带空格路径被拆参。
//!
//! **坑**：`RegSetValueExW` 是宽字符版 API，`REG_SZ` 的数据必须是 **UTF-16LE 字节序列**
//! （含结尾 null）。早期版本直接塞了 `String::as_bytes()`（UTF-8），注册表按 UTF-16
//! 存下来就是乱码路径，开机时 Windows 运行失败 —— 表现为「开关显示已开启但不生效」。

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

/// 期望写入的自启值：带引号的 exe 全路径
fn expected_value() -> Option<String> {
    std::env::current_exe()
        .ok()
        .map(|p| format!("\"{}\"", p.to_string_lossy()))
}

/// 读取 Run 项里现存的值（REG_SZ，UTF-16LE → String）
fn read_value() -> Option<String> {
    unsafe {
        let mut hkey: isize = 0;
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            wide(RUN_KEY).as_ptr(),
            0,
            KEY_READ,
            &mut hkey,
        ) != ERROR_SUCCESS
        {
            return None;
        }
        let mut typ: u32 = 0;
        let mut cb: u32 = 0;
        // 先按 null 缓冲调用一次拿字节长度
        if RegQueryValueExW(
            hkey,
            wide(VALUE_NAME).as_ptr(),
            std::ptr::null_mut(),
            &mut typ,
            std::ptr::null_mut(),
            &mut cb,
        ) != ERROR_SUCCESS
        {
            RegCloseKey(hkey);
            return None;
        }
        let mut buf = vec![0u8; cb as usize];
        let r = RegQueryValueExW(
            hkey,
            wide(VALUE_NAME).as_ptr(),
            std::ptr::null_mut(),
            &mut typ,
            buf.as_mut_ptr(),
            &mut cb,
        );
        RegCloseKey(hkey);
        if r != ERROR_SUCCESS {
            return None;
        }
        let units: Vec<u16> = buf
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
        Some(String::from_utf16_lossy(&units[..end]))
    }
}

/// 是否已开启自启：不只看「值存在」，还要求内容就是当前 exe 的路径。
/// 这样 exe 被移动/重装后状态依然准确，也不会把写坏的值当成已开启。
pub fn is_enabled() -> bool {
    let Some(expected) = expected_value() else {
        return false;
    };
    read_value().as_deref() == Some(expected.as_str())
}

/// 启动自愈：修复旧版本用 UTF-8 字节写坏的 REG_SZ 值。
///
/// 老用户的注册表里已经躺着一条乱码路径，即使换了新 exe 也不会自愈 ——
/// 这里在每次启动时比对一次，值存在但内容不符就重写。
pub fn repair_if_needed() {
    let Some(expected) = expected_value() else {
        return;
    };
    match read_value() {
        Some(v) if v != expected => {
            println!("[autostart] 自启项内容不是当前 exe 路径，重写修复");
            let _ = set_enabled(true);
        }
        _ => {}
    }
}

pub fn set_enabled(enable: bool) -> Result<(), String> {
    if enable {
        let Some(data) = expected_value() else {
            return Err("取 exe 路径失败".into());
        };
        // RegSetValueExW 是宽字符版 API：REG_SZ 的数据必须是 UTF-16LE 字节序列（含结尾 null）。
        // 直接塞 UTF-8 字节会被注册表按 UTF-16 解释成乱码路径，开机时运行失败。
        let mut wide_data: Vec<u16> = data.encode_utf16().collect();
        wide_data.push(0);
        let bytes = unsafe {
            std::slice::from_raw_parts(wide_data.as_ptr() as *const u8, wide_data.len() * 2)
        };

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
