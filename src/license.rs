//! 授权文件（license）复用逻辑。
//!
//! 讯飞设备级授权是按“设备数 × 有效期”计量的：**每次在线激活都会占用一个装机量**。
//! SDK 首次激活后会把授权缓存到 `<workDir>/<appid><32位hex>`，之后即使断网也能用
//! （已实测：`unshare -n` 无网络环境下可正常合成）。
//!
//! 所以本项目固定复用同一个 workDir 与同一份授权文件：
//!   1. workDir 里已有 `<appid><hex>` → 直接复用，SDK 不会联网；
//!   2. workDir 里没有，但配置/包目录里有一份 → 复制进去（**只复制，不移动**，
//!      源文件永远保留；注意直接把它交给 SDK 的 licenseFile 参数会被 SDK 搬走甚至重写）；
//!   3. 都没有 → 由 SDK 联网做首次激活（占 1 个装机量）；若配置了 offline_only 则直接报错。

use std::path::{Path, PathBuf};

use crate::Config;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LicenseMode {
    /// workDir 中已有缓存授权，复用
    Cached,
    /// 从别处复制了一份到 workDir
    Seeded,
    /// 交给 SDK 联网首次激活
    OnlineActivation,
}

#[derive(Debug, Clone)]
pub struct License {
    pub mode: LicenseMode,
    /// workDir 中的授权文件路径（OnlineActivation 时为 None）
    pub path: Option<PathBuf>,
    /// 复制来源
    pub source: Option<PathBuf>,
    /// 需要交给 SDK `licenseFile` 参数的临时文件（非缓存文件名时用）
    pub seed_file: Option<PathBuf>,
}

impl License {
    pub fn describe(&self) -> String {
        match self.mode {
            LicenseMode::Cached => format!(
                "复用 workDir 中已有授权：{}",
                self.path.as_ref().map(|p| p.display().to_string()).unwrap_or_default()
            ),
            LicenseMode::Seeded => format!(
                "已把授权文件 {} 复制到 {}",
                self.source.as_ref().map(|p| p.display().to_string()).unwrap_or_default(),
                self.path.as_ref().map(|p| p.display().to_string()).unwrap_or_default()
            ),
            LicenseMode::OnlineActivation => "未找到可用授权文件，将联网做首次激活（会占用 1 个装机量）".to_string(),
        }
    }
}

/// SDK 的授权缓存文件名：`<appid><32 位十六进制>`。
pub fn is_stored_name(name: &str, app_id: &str) -> bool {
    let Some(rest) = name.strip_prefix(app_id) else {
        return false;
    };
    rest.len() == 32 && rest.chars().all(|c| c.is_ascii_hexdigit())
}

/// 在目录里找 SDK 缓存形式的授权文件。
pub fn find_stored(dir: &Path, app_id: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !is_stored_name(&name, app_id) {
            continue;
        }
        let mtime = e
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        if best.as_ref().map(|(t, _)| mtime > *t).unwrap_or(true) {
            best = Some((mtime, e.path()));
        }
    }
    best.map(|(_, p)| p)
}

/// 粗判一个文件是否像 AIkit 授权文件（JSON，含 device/extension/license 三个字段）。
pub fn looks_like_license(path: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    text.contains("\"device\"") && text.contains("\"license\"") && text.contains("\"extension\"")
}

/// 计算授权文件短指纹，用于日志比对（不泄露内容）。
pub fn digest(path: &Path) -> String {
    let Ok(data) = std::fs::read(path) else {
        return "-".to_string();
    };
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in &data {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    format!("{:016x}/{}B", hash, data.len())
}

/// 按上面三条规则准备授权文件。
pub fn prepare(cfg: &Config, app_id: &str) -> Result<License, String> {
    std::fs::create_dir_all(&cfg.work_dir)
        .map_err(|e| format!("无法创建 workDir {}: {e}", cfg.work_dir.display()))?;

    if let Some(p) = find_stored(&cfg.work_dir, app_id) {
        return Ok(License {
            mode: LicenseMode::Cached,
            path: Some(p),
            source: None,
            seed_file: None,
        });
    }

    // 候选来源：配置里的 XfyunLicenseFile → SDK 包目录（workDir 本身一开始就查过了）
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(p) = cfg.license_file.clone() {
        candidates.push(p);
    }
    if let Some(p) = find_stored(&cfg.sdk_root, app_id) {
        candidates.push(p);
    }
    for src in candidates {
        if !src.is_file() {
            continue;
        }
        if !looks_like_license(&src) {
            crate::log_warn!("{} 看起来不是 AIkit 授权文件，跳过", src.display());
            continue;
        }
        let file_name = src.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        if is_stored_name(&file_name, app_id) {
            // 文件名就是 SDK 期待的形式：直接复制进 workDir，源文件保留
            let dst = cfg.work_dir.join(&file_name);
            std::fs::copy(&src, &dst)
                .map_err(|e| format!("复制授权文件到 {} 失败: {e}", dst.display()))?;
            return Ok(License {
                mode: LicenseMode::Seeded,
                path: Some(dst),
                source: Some(src),
                seed_file: None,
            });
        }
        // 任意文件名的离线激活文件：放到 workDir 下的临时名，交给 SDK 的 licenseFile
        // 参数（SDK 会把它转换成缓存名，也就是会“搬走”这个临时文件，所以必须是副本）。
        let seed = cfg.work_dir.join("license-seed.json");
        std::fs::copy(&src, &seed)
            .map_err(|e| format!("准备离线激活文件 {} 失败: {e}", seed.display()))?;
        crate::log_info!("使用离线激活文件 {}", src.display());
        return Ok(License {
            mode: LicenseMode::Seeded,
            path: None,
            source: Some(src),
            seed_file: Some(seed),
        });
    }

    if cfg.offline_only {
        return Err(format!(
            "未找到授权文件，且已开启 XfyunOfflineOnly（拒绝联网激活）。\
             请把 SDK 的授权文件放到 {}，或用 XfyunLicenseFile 指定；\
             原包里的授权文件形如 {}/{}<32位hex>",
            cfg.work_dir.display(),
            cfg.sdk_root.display(),
            app_id
        ));
    }
    Ok(License { mode: LicenseMode::OnlineActivation, path: None, source: None, seed_file: None })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_name_detection() {
        assert!(is_stored_name("aa2bd560cf55de4ddc7a1220817d7c0cdbb1fd90", "aa2bd560"));
        assert!(!is_stored_name("aa2bd560", "aa2bd560"));
        assert!(!is_stored_name("aa2bd560ZZ55de4ddc7a1220817d7c0cdbb1fd90", "aa2bd560"));
        assert!(!is_stored_name("bb2bd560cf55de4ddc7a1220817d7c0cdbb1fd90", "aa2bd560"));
    }

    #[test]
    fn finds_newest_stored_license() {
        let dir = std::env::temp_dir().join(format!("xfyun-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let old = dir.join("aa2bd560cf55de4ddc7a1220817d7c0cdbb1fd90");
        std::fs::write(&old, "{}").unwrap();
        std::fs::write(dir.join("not-a-license.txt"), "x").unwrap();
        let found = find_stored(&dir, "aa2bd560").unwrap();
        assert_eq!(found, old);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rejects_non_license_file() {
        let p = std::env::temp_dir().join(format!("xfyun-notlic-{}", std::process::id()));
        std::fs::write(&p, "hello").unwrap();
        assert!(!looks_like_license(&p));
        std::fs::remove_file(&p).ok();
    }
}
