//! 让引擎动态库能被 SDK `dlopen` 到。
//!
//! 实测结论（strace 验证）：`libaikit.so` 用**裸文件名** `dlopen` 引擎库
//! （如 `ebd1bade4_v1025_aee.so`），既不扫描 workDir，也不理会可执行文件的
//! **DT_RPATH**（libaikit 自己有 DT_RUNPATH，glibc 在 dlopen 时不会用调用方的 RUNPATH）。
//! 因此只有 `LD_LIBRARY_PATH` / `ld.so.cache` / 默认目录能生效 —— 否则报 18201 引擎加载失败。
//!
//! `LD_LIBRARY_PATH` 只在进程启动时读取，运行时修改无效，所以这里在 `main()` 最开头
//! 重新 exec 自身一次（execv 保持 PID 与已打开的文件描述符，对 speech-dispatcher 透明）。

use std::path::{Path, PathBuf};
use std::process::Command;

use std::os::unix::process::CommandExt;

const REEXEC_FLAG: &str = "SD_XFYUN_REEXEC";

/// 引擎库文件名形如 `<hash>_v1025_aee.so`。
pub fn engine_lib(libs_dir: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(libs_dir).ok()?;
    let mut fallback = None;
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".so") {
            continue;
        }
        if name.contains("_aee") {
            return Some(e.path());
        }
        if name != "libaikit.so" {
            fallback = Some(e.path());
        }
    }
    fallback
}

fn path_contains(var: &str, dir: &Path) -> bool {
    let Ok(value) = std::env::var(var) else {
        return false;
    };
    let dir = dir.to_string_lossy();
    value.split(':').any(|p| !p.is_empty() && Path::new(p) == Path::new(dir.as_ref()))
}

/// 是否已经重新 exec 过（防止死循环）。
fn already_reexeced() -> bool {
    std::env::var(REEXEC_FLAG).map(|v| v == "1").unwrap_or(false)
}

/// 必要时重新 exec 自身，让 `libs_dir` 进入 `LD_LIBRARY_PATH`。
///
/// 返回 `Ok(())` 表示可以继续执行（要么已经能加载，要么已经 re-exec 过一轮）。
pub fn ensure_engine_loadable(libs_dir: &Path, enabled: bool, argv: &[String]) -> Result<(), String> {
    if !libs_dir.is_dir() {
        return Err(format!("动态库目录不存在：{}（请检查 XfyunLibsDir / XFYUN_SDK_ROOT）", libs_dir.display()));
    }
    if engine_lib(libs_dir).is_none() {
        crate::log_warn!("在 {} 里没找到引擎动态库（*_aee.so），SDK 初始化可能返回 18201", libs_dir.display());
    }
    if !enabled || path_contains("LD_LIBRARY_PATH", libs_dir) {
        return Ok(());
    }
    if already_reexeced() {
        crate::log_debug!("已 re-exec 过，跳过（LD_LIBRARY_PATH={:?}）", std::env::var("LD_LIBRARY_PATH"));
        return Ok(());
    }

    let old = std::env::var("LD_LIBRARY_PATH").unwrap_or_default();
    let new_path = if old.is_empty() {
        libs_dir.to_string_lossy().into_owned()
    } else {
        format!("{}:{}", libs_dir.display(), old)
    };
    let exe = std::env::current_exe().map_err(|e| format!("无法获取自身路径: {e}"))?;
    crate::log_debug!("为加载引擎动态库重新启动：LD_LIBRARY_PATH={new_path}");

    let err = Command::new(&exe)
        .args(argv)
        .env("LD_LIBRARY_PATH", &new_path)
        .env(REEXEC_FLAG, "1")
        .exec();
    // exec 成功就不会返回
    Err(format!("re-exec {} 失败: {err}", exe.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_aee_engine_library() {
        let dir = std::env::temp_dir().join(format!("xfyun-libs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("libaikit.so"), "").unwrap();
        std::fs::write(dir.join("ebd1bade4_v1025_aee.so"), "").unwrap();
        assert!(engine_lib(&dir).unwrap().ends_with("ebd1bade4_v1025_aee.so"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn path_contains_detects_dir() {
        std::env::set_var("XFYUN_TEST_LDPATH", "/a/b:/c/d");
        assert!(path_contains("XFYUN_TEST_LDPATH", Path::new("/c/d")));
        assert!(!path_contains("XFYUN_TEST_LDPATH", Path::new("/c")));
    }
}
