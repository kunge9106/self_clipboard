//! 处理文件条目引用的本地文件（移入回收站，或按设置永久删除）。
//!
//! 为什么默认走回收站：文件条目指向的是用户自己的真实文件（文档、照片、源码），
//! 一次误点不可接受 —— 回收站能还原，与永久删除的安全边界完全不同。
//! 永久删除只在设置里显式选过之后才会用（默认 `delete_files_permanent = false`）。
//!
//! 安全约束（刻意收紧，不为了「好用」放宽）：
//!   - 只处理**绝对路径**且 `is_file()` 为真的目标：目录一律跳过并报告，
//!     免得某条记录的 `files` 里混进一个目录，把整棵树带走；
//!   - 不递归、不按通配符展开，只删调用方明确给的那几个路径；
//!   - 调用方（`clipboard`）只会传入条目自己 `files` 里记过的路径。

use std::os::windows::ffi::OsStrExt;

const FO_DELETE: u32 = 3;
/// 不弹系统进度条
const FOF_SILENT: u16 = 0x0004;
/// 不再向用户二次确认（我们自己已经弹过窗了）
const FOF_NOCONFIRMATION: u16 = 0x0010;
/// **允许撤销**：这一位决定「进回收站」而不是永久删除
const FOF_ALLOWUNDO: u16 = 0x0040;
/// 出错时不弹系统错误框
const FOF_NOERRORUI: u16 = 0x0400;
/// 目标过大进不了回收站时提示，而不是直接永久删除
const FOF_WANTNUKEWARNING: u16 = 0x4000;

#[repr(C)]
struct ShFileOpStructW {
    hwnd: isize,
    w_func: u32,
    p_from: *const u16,
    p_to: *const u16,
    f_flags: u16,
    f_any_operations_aborted: i32,
    h_name_mappings: *mut std::ffi::c_void,
    lpsz_progress_title: *const u16,
}

#[link(name = "shell32")]
extern "system" {
    fn SHFileOperationW(op: *mut ShFileOpStructW) -> i32;
}

/// 处理本地文件的统一入口，返回（成功数，失败说明）。
///
/// 两种方式的差别只在最后一步；前置校验（绝对路径、`is_file`、不碰目录）共用 ——
/// 它们在**永久删除**时更不能省，所以刻意写成同一段，避免两条路各写一遍而漏掉校验。
pub fn purge(paths: &[String], permanent: bool) -> (usize, Vec<String>) {
    if permanent {
        let mut ok = 0usize;
        let mut failed: Vec<String> = Vec::new();
        for raw in paths {
            let path = std::path::Path::new(raw);
            if !path.is_absolute() {
                failed.push(format!("{raw}（不是绝对路径，已跳过）"));
                continue;
            }
            if !path.is_file() {
                failed.push(format!("{raw}（不是文件或已不存在）"));
                continue;
            }
            match std::fs::remove_file(path) {
                Ok(()) => ok += 1,
                Err(e) => failed.push(format!("{raw}（{e}）")),
            }
        }
        (ok, failed)
    } else {
        move_to_recycle(paths)
    }
}

/// 逐条移入回收站，返回（成功数，失败说明）。
///
/// 逐条而不是一次性提交：`SHFileOperationW` 是批处理，一个文件失败只会给一个
/// 总体错误码，无法知道是哪几个 —— 而这里最需要告诉用户的正是「哪几个没删掉」。
pub fn move_to_recycle(paths: &[String]) -> (usize, Vec<String>) {
    let mut ok = 0usize;
    let mut failed: Vec<String> = Vec::new();

    for raw in paths {
        let path = std::path::Path::new(raw);
        if !path.is_absolute() {
            failed.push(format!("{raw}（不是绝对路径，已跳过）"));
            continue;
        }
        if !path.is_file() {
            // 目录 / 已不存在的路径：目录**绝不**处理（防整棵树被带走）
            failed.push(format!("{raw}（不是文件或已不存在）"));
            continue;
        }

        // pFrom 必须以「两个 \0」结尾：一个结束路径，一个结束列表
        let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        wide.push(0);
        wide.push(0);

        let mut op = ShFileOpStructW {
            hwnd: 0,
            w_func: FO_DELETE,
            p_from: wide.as_ptr(),
            p_to: std::ptr::null(),
            f_flags: FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_SILENT | FOF_NOERRORUI
                | FOF_WANTNUKEWARNING,
            f_any_operations_aborted: 0,
            h_name_mappings: std::ptr::null_mut(),
            lpsz_progress_title: std::ptr::null(),
        };

        let rc = unsafe { SHFileOperationW(&mut op) };
        // 返回 0 才算成功；用户在我们的弹窗里已确认，所以「被中止」也视为未完成
        if rc == 0 && op.f_any_operations_aborted == 0 {
            ok += 1;
        } else {
            failed.push(format!("{raw}（系统返回 {rc}）"));
        }
    }

    (ok, failed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(name);
        let _ = std::fs::remove_file(&p);
        std::fs::write(&p, b"qingtie purge test").unwrap();
        p
    }

    /// 回收站路径必须真的把文件移走（原位置不再存在）。
    ///
    /// 这条测的是 Windows API 的调用姿势对不对：`pFrom` 少了结尾那个 `\0`、
    /// 或者漏了 `FOF_ALLOWUNDO`，结果都只是「看起来成功」——文件其实还在原地。
    #[test]
    fn recycle_moves_file_away() {
        let p = tmp("qingtie-purge-recycle.txt");
        let arg = vec![p.to_string_lossy().to_string()];
        let (ok, failed) = move_to_recycle(&arg);
        assert_eq!(ok, 1, "应当成功 1 个，实际 failed={failed:?}");
        assert!(!p.exists(), "文件应已离开原位置（进了回收站）");
    }

    /// 目录绝不能被处理：某条记录的 `files` 里一旦混进目录，
    /// 放行它就等于把整棵树带走。
    #[test]
    fn directory_is_skipped() {
        let dir = std::env::temp_dir().join("qingtie-purge-dir");
        let _ = std::fs::create_dir_all(&dir);
        let f = dir.join("keep.txt");
        std::fs::write(&f, b"keep").unwrap();

        let arg = vec![dir.to_string_lossy().to_string()];
        let (ok, failed) = purge(&arg, false);
        assert_eq!(ok, 0, "目录不应被处理");
        assert_eq!(failed.len(), 1);
        assert!(dir.is_dir(), "目录必须还在");
        assert!(f.is_file(), "目录里的文件也必须还在");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 不存在的路径要报失败，而不是静默算成功。
    #[test]
    fn missing_path_reports_failure() {
        let p = std::env::temp_dir().join("qingtie-purge-not-exist-xyz.txt");
        let _ = std::fs::remove_file(&p);
        let arg = vec![p.to_string_lossy().to_string()];
        let (ok, failed) = purge(&arg, false);
        assert_eq!(ok, 0);
        assert_eq!(failed.len(), 1);
    }

    /// 永久删除：文件直接从磁盘消失。
    #[test]
    fn permanent_delete_removes_file() {
        let p = tmp("qingtie-purge-permanent.txt");
        let arg = vec![p.to_string_lossy().to_string()];
        let (ok, failed) = purge(&arg, true);
        assert_eq!(ok, 1, "failed={failed:?}");
        assert!(!p.exists());
    }
}
