// 隐藏控制台窗口（仅 release）
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    self_clipboard_lib::run()
}
