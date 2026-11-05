//! 处理文件条目引用的本地文件（移入回收站，或按设置永久删除）。
//!
//! 为什么默认走回收站：文件条目指向的是用户自己的真实文件（文档、照片、源码），
//! 一次误点不可接受 —— 回收站能还原，与永久删除的安全边界完全不同。
//! 永久删除只在设置里显式选过之后才会用（默认 `delete_files_permanent = false`）。
//!
//! 安全约束（刻意收紧，不为了「好用」放宽）：
//!   - 只处理**绝对路径**且确实存在的目标，非绝对路径一律拒绝并报告；
//!   - **目录只进回收站，永不递归永久删除**：`permanent` 模式下目录也改走回收站，
//!     因为递归删除不可逆，一次判断错就是整棵树没了；
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

/// 一次处理的分类结果。
///
/// 之前返回 `(永久删除数, 回收站数, Vec<String>)`，调用方靠
/// `contains("已不存在")` 从失败文案里反推「是没删成、还是本来就没有」——
/// 改一句文案分类就会失灵，日志与用户提示一起出错（代码审查 E2′）。
/// 改为结构化数据后，文案怎么改都不影响归类。
#[derive(Default)]
pub struct PurgeReport {
    /// 永久删除的文件数
    pub deleted: usize,
    /// 移入回收站的数量（文件 + 文件夹）
    pub recycled: usize,
    /// 目标原本就不存在：目的已达成，**不算失败**
    pub missing: usize,
    /// 真失败（权限、占用、非绝对路径等）
    pub failures: Vec<PurgeFailure>,
}

/// 一条失败记录。
pub struct PurgeFailure {
    /// 出错的路径。**只用于开发版日志** —— 路径里带着文件名，属于内容级信息，
    /// 发布版不写进日志（见 `clipboard::purge_local_files`）。
    pub path: String,
    pub kind: FailureKind,
}

/// 失败原因。分类而不是拼字符串，调用方据此决定怎么说、要不要记 WARN。
pub enum FailureKind {
    /// 不是绝对路径：索引损坏或调用方构造错误
    NotAbsolute,
    /// 系统拒绝，附原始描述（权限、占用、回收站拒绝等）
    Denied(String),
}

/// 两条分支共用的前置校验：必须是绝对路径，且真的存在。
///
/// `Ok(Some)` 可处理 / `Ok(None)` 已不存在（调用方计入 `missing`）/ `Err` 不可处理。
/// 抽出来是因为「什么算可处理目标」必须只有一份定义 —— 永久删除与回收站两条分支
/// 各写一遍时，任何一侧加规则（拒绝 UNC、拒绝符号链接、拒绝系统目录…）都只会改一半，
/// 行为分叉后极难发现（审查 §4）。目录检查**不**在这里：永久删除拒绝目录，
/// 回收站放行目录，这是两条分支的真实差异，不属于公共部分。
fn precheck(raw: &str) -> Result<Option<std::path::PathBuf>, PurgeFailure> {
    let path = std::path::PathBuf::from(raw);
    if !path.is_absolute() {
        return Err(PurgeFailure {
            path: raw.to_string(),
            kind: FailureKind::NotAbsolute,
        });
    }
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(path))
}

/// 处理一批本地路径。返回（永久删除数，移入回收站数，失败说明）。
///
/// **目录始终走回收站**，与 `permanent` 无关 —— 递归永久删除不可逆，一次判断错
/// 就是整棵树没了；回收站随时能还原。这条只对**本次调用**生效，不改用户的
/// 「删除方式」设置（实测反馈：不能让用户为删一个文件夹去改全局设置）。
pub fn purge(paths: &[String], permanent: bool) -> PurgeReport {
    if !permanent {
        // 全部走回收站：文件与文件夹都算「已移入回收站」，调用方文案不区分类型
        return move_to_recycle(paths);
    }

    let mut report = PurgeReport::default();
    for raw in paths {
        let path = match precheck(raw) {
            Ok(Some(p)) => p,
            Ok(None) => {
                report.missing += 1;
                continue;
            }
            Err(f) => {
                report.failures.push(f);
                continue;
            }
        };
        if path.is_dir() {
            // 本次单独按回收站处理，不碰全局设置
            let sub = move_to_recycle(std::slice::from_ref(raw));
            report.recycled += sub.recycled;
            report.missing += sub.missing;
            report.failures.extend(sub.failures);
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => report.deleted += 1,
            Err(e) => report.failures.push(PurgeFailure {
                path: raw.clone(),
                kind: FailureKind::Denied(e.to_string()),
            }),
        }
    }
    report
}

/// 逐条移入回收站。
///
/// 逐条而不是一次性提交：`SHFileOperationW` 是批处理，一个文件失败只会给一个
/// 总体错误码，无法知道是哪几个 —— 而这里最需要告诉用户的正是「哪几个没删掉」。
pub fn move_to_recycle(paths: &[String]) -> PurgeReport {
    let mut report = PurgeReport::default();

    for raw in paths {
        let path = match precheck(raw) {
            Ok(Some(p)) => p,
            Ok(None) => {
                report.missing += 1;
                continue;
            }
            Err(f) => {
                report.failures.push(f);
                continue;
            }
        };
        // 目录也放行：`SHFileOperationW` + `FOF_ALLOWUNDO` 会把整棵目录树
        // 移进回收站，用户随时可以还原 —— 这比「删不掉」和「永久递归删除」
        // 都更合适。永久删除那条分支仍然拒绝目录（在 `purge` 里）。

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
            report.recycled += 1;
        } else {
            report.failures.push(PurgeFailure {
                path: raw.clone(),
                kind: FailureKind::Denied(format!("系统返回 {rc}")),
            });
        }
    }

    report
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
        let r = move_to_recycle(&arg);
        assert_eq!(r.recycled, 1, "应当成功 1 个，实际 failures={:?}", r.failures.len());
        assert!(!p.exists(), "文件应已离开原位置（进了回收站）");
    }

    /// 目录在「永久删除」模式下**改走回收站**，且只影响本次调用。
    ///
    /// `#[ignore]`：它会真的把临时目录移进用户的回收站，日常测试不该有这种副作用。
    /// 需要验证时手动跑 `cargo test -- --ignored`。
    #[test]
    #[ignore = "会往用户回收站里塞一个临时目录"]
    fn directory_goes_to_recycle_even_in_permanent_mode() {
        let dir = std::env::temp_dir().join("qingtie-purge-dir-recycle");
        let _ = std::fs::create_dir_all(&dir);
        let f = dir.join("keep.txt");
        std::fs::write(&f, b"keep").unwrap();

        let arg = vec![dir.to_string_lossy().to_string()];
        let r = purge(&arg, true);
        assert_eq!(r.deleted, 0, "目录不能被永久删除");
        assert_eq!(r.recycled, 1, "目录应改走回收站");
        assert!(r.failures.is_empty(), "不应报失败：{}", r.failures.len());
        assert!(!dir.exists(), "目录应已移入回收站");
    }

    /// 不存在的路径算 `missing`，不算失败。
    ///
    /// 语义上「目标已不存在」= 目的达成；把它和权限/占用混在一起，用户会收到
    /// 「1 个未能处理」并去查权限，方向完全错（实测反馈）。但它**也不能**被
    /// 静默算成「我们删掉的」—— 所以单独计数、单独说明，而不是并进成功。
    #[test]
    fn missing_path_is_counted_separately_from_failures() {
        let p = std::env::temp_dir().join("qingtie-purge-not-exist-xyz.txt");
        let _ = std::fs::remove_file(&p);
        let arg = vec![p.to_string_lossy().to_string()];
        let r = purge(&arg, false);
        assert_eq!(r.recycled, 0);
        assert_eq!(r.missing, 1, "应计入 missing");
        assert!(r.failures.is_empty(), "missing 不应进 failures");
    }

    /// 非绝对路径算失败，且分类是结构化的（不靠文案字符串反推）。
    #[test]
    fn relative_path_is_a_structural_failure() {
        let arg = vec!["relative\\path.txt".to_string()];
        let r = purge(&arg, true);
        assert_eq!(r.missing, 0);
        assert_eq!(r.failures.len(), 1);
        assert!(matches!(r.failures[0].kind, FailureKind::NotAbsolute));
        assert_eq!(r.failures[0].path, "relative\\path.txt", "失败项要保留路径供开发版日志");
    }

    /// 永久删除：文件直接从磁盘消失，且**不会**被算进回收站计数。
    ///
    /// 这条守住 S2 的根因：非永久模式下所有成功项都进 `recycled`，永久模式下
    /// 只有目录进 `recycled` —— 调用方要据此选文案，不能一律说「文件夹」。
    #[test]
    fn permanent_delete_removes_file_and_never_counts_as_recycled() {
        let p = tmp("qingtie-purge-permanent.txt");
        let arg = vec![p.to_string_lossy().to_string()];
        let r = purge(&arg, true);
        assert_eq!(r.deleted, 1, "failures={}", r.failures.len());
        assert_eq!(r.recycled, 0, "文件不该被算作「移入回收站」");
        assert!(!p.exists());
    }
}
