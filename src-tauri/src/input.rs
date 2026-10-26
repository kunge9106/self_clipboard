//! 键盘注入（`SendInput`）的**唯一入口**。
//!
//! 面板粘贴要发 Ctrl+V，截图右键取消要发 Esc。这两处如果各自定义一套 `INPUT`
//! 结构体，将来改结构体（或加扫描码支持）必然漏掉一处，表现是「某个功能的按键
//! 静默失效」—— 既没有报错也不影响其它功能，是最难定位的一类问题。
//!
//! `INPUT` 里那个 union 的大小必须与系统一致，错了 `SendInput` 会直接返回 0，
//! 所以这里保留真实的最大成员 `MOUSEINPUT` 占位，而不是自己算字节数。

const INPUT_KEYBOARD: u32 = 1;
const KEYEVENTF_KEYUP: u32 = 0x0002;

#[repr(C)]
#[derive(Clone, Copy)]
struct MouseInput {
    dx: i32,
    dy: i32,
    mouse_data: u32,
    flags: u32,
    time: u32,
    extra: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct KeybdInput {
    vk: u16,
    scan: u16,
    flags: u32,
    time: u32,
    extra: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct HardwareInput {
    msg: u32,
    param_l: u16,
    param_h: u16,
}

#[repr(C)]
union InputUnion {
    /// 只用来撑起 union 的大小（实际不会发送鼠标事件）
    #[allow(dead_code)]
    mi: MouseInput,
    ki: KeybdInput,
    #[allow(dead_code)]
    hi: HardwareInput,
}

#[repr(C)]
struct Input {
    ty: u32,
    u: InputUnion,
}

extern "system" {
    fn SendInput(count: u32, inputs: *const Input, size: i32) -> u32;
}

/// 按顺序按下 `keys`，再**逆序**抬起（组合键的标准顺序）。
///
/// 一次 `SendInput` 提交全部事件，中间不会被真实输入插进来 —— 分多次发的话，
/// 高负载时可能被别的按键夹在中间，出现「Ctrl 还没抬起就来了个别的键」。
///
/// 返回是否全部送出。调用方只在失败时记日志，不弹窗：这类失败几乎只有
/// 「目标进程权限更高（UAC 提权窗口）」一种原因，用户也做不了什么。
pub fn send_keys(keys: &[u16]) -> bool {
    let mut seq: Vec<Input> = Vec::with_capacity(keys.len() * 2);
    for vk in keys {
        seq.push(event(*vk, false));
    }
    for vk in keys.iter().rev() {
        seq.push(event(*vk, true));
    }
    let size = std::mem::size_of::<Input>() as i32;
    let sent = unsafe { SendInput(seq.len() as u32, seq.as_ptr(), size) };
    sent == seq.len() as u32
}

fn event(vk: u16, up: bool) -> Input {
    Input {
        ty: INPUT_KEYBOARD,
        u: InputUnion {
            ki: KeybdInput {
                vk,
                scan: 0,
                flags: if up { KEYEVENTF_KEYUP } else { 0 },
                time: 0,
                extra: 0,
            },
        },
    }
}
