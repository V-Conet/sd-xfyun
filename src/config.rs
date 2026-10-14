//! 配置：既能解析 speech-dispatcher 传给模块的配置文件（dotconf 风格 `Key value`），
//! 也能被命令行参数覆盖，另有环境变量兜底。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::keys;

#[derive(Debug, Clone)]
pub struct Config {
    /// SDK 包根目录（含 include/、libs/、xtts/）
    pub sdk_root: PathBuf,
    /// 存放 libaikit.so 与 *\_aee.so 引擎库的目录
    pub libs_dir: PathBuf,
    /// 资源目录（其下应有 xtts/），默认 sdk_root
    pub res_dir: PathBuf,
    /// SDK 工作目录：授权 license 缓存、日志都落在这里，必须可写
    pub work_dir: PathBuf,
    /// 授权文件（离线激活文件或已缓存的 license）来源，会被复制到 work_dir
    pub license_file: Option<PathBuf>,
    /// 只允许离线：没有可用授权文件时拒绝联网激活（避免消耗装机量）
    pub offline_only: bool,
    pub default_voice: String,
    pub default_language: String,
    pub speed: u8,
    pub pitch: u8,
    pub volume: u8,
    pub max_chunk: usize,
    pub log_level: u8,
    pub log_file: Option<PathBuf>,
    /// 外部播放器（自播放模式）；为空则自动探测
    pub player: Option<String>,
    /// 是否接受由 speech-dispatcher 服务端播放音频（705 协议）
    pub server_audio: bool,
    /// 授权检查间隔（秒），默认 1 小时；SDK 默认 300 秒
    pub auth_check_interval: u32,
    /// 启动时用 LD_LIBRARY_PATH 重新 exec 自身，让引擎动态库可被 dlopen
    pub reexec: bool,
    /// 未知配置项（保留，便于诊断）
    pub unknown: BTreeMap<String, String>,
}

impl Default for Config {
    fn default() -> Self {
        let sdk_root = default_sdk_root();
        Config {
            libs_dir: sdk_root.join("libs"),
            res_dir: sdk_root.clone(),
            sdk_root,
            work_dir: default_work_dir(),
            license_file: None,
            offline_only: false,
            default_voice: "xiaoyan".to_string(),
            default_language: "zh-CN".to_string(),
            speed: 50,
            pitch: 50,
            volume: 50,
            max_chunk: 120,
            log_level: crate::log::WARN,
            log_file: None,
            player: None,
            server_audio: true,
            auth_check_interval: 3600,
            reexec: true,
            unknown: BTreeMap::new(),
        }
    }
}

/// 默认 SDK 根目录：构建期探测到的那个（`3rd_party/<SDK 包>`），可用配置/环境变量覆盖。
fn default_sdk_root() -> PathBuf {
    // 构建期探测到的 SDK 包目录（开发树是 <项目>/3rd_party/<SDK 包>，打包安装是 /usr/share/sd-xfyun）。
    // 不做任何「cwd / 上级目录」之类的猜测：路径不对时由 XfyunSdkRoot/XfyunResDir 显式指定，
    // 或者直接看 18105/18201 的报错提示。
    PathBuf::from(keys::compiled_sdk_root())
}

fn xdg_data_home() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_DATA_HOME") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            return PathBuf::from(home).join(".local/share");
        }
    }
    PathBuf::from("/tmp")
}

fn default_work_dir() -> PathBuf {
    xdg_data_home().join("sd-xfyun/work")
}

impl Config {
    /// 从模块配置文件（可能不存在）加载。
    pub fn load(path: Option<&Path>) -> Config {
        let mut cfg = Config::default();
        if let Some(p) = path {
            match std::fs::read_to_string(p) {
                Ok(text) => cfg.apply_conf(&text),
                Err(e) => crate::log_warn!("读取配置文件 {} 失败: {e}", p.display()),
            }
        }
        cfg.apply_env();
        cfg.normalize();
        cfg
    }

    /// 解析 dotconf 风格配置：`Key value` / `Key "value"`，`#` 起注释。
    pub fn apply_conf(&mut self, text: &str) {
        for (lineno, raw) in text.lines().enumerate() {
            let line = strip_comment(raw).trim();
            if line.is_empty() {
                continue;
            }
            let mut it = line.splitn(2, char::is_whitespace);
            let key = it.next().unwrap_or("").trim();
            let value = it.next().unwrap_or("").trim();
            let value = unquote(value);
            if key.is_empty() {
                continue;
            }
            if !self.set(key, &value) {
                self.unknown.insert(key.to_string(), value.clone());
                crate::log_debug!("配置项 {} 未识别（第 {} 行）", key, lineno + 1);
            }
        }
    }

    /// 单个配置项赋值；返回 false 表示未知键。
    pub fn set(&mut self, key: &str, value: &str) -> bool {
        let path = |v: &str| PathBuf::from(v);
        match key {
            "XfyunSdkRoot" | "SdkRoot" => self.sdk_root = path(value),
            "XfyunLibsDir" | "LibsDir" => self.libs_dir = path(value),
            "XfyunResDir" | "ResDir" => self.res_dir = path(value),
            "XfyunWorkDir" | "WorkDir" => self.work_dir = path(value),
            "XfyunLicenseFile" | "LicenseFile" => {
                self.license_file = if value.is_empty() { None } else { Some(path(value)) }
            }
            "XfyunOfflineOnly" | "OfflineOnly" => self.offline_only = truthy(value),
            "XfyunDefaultVoice" | "DefaultVoice" => self.default_voice = value.to_string(),
            "XfyunDefaultLanguage" | "DefaultLanguage" => self.default_language = value.to_string(),
            "XfyunSpeed" | "DefaultSpeed" => self.speed = clamp_u8(value, self.speed),
            "XfyunPitch" | "DefaultPitch" => self.pitch = clamp_u8(value, self.pitch),
            "XfyunVolume" | "DefaultVolume" => self.volume = clamp_u8(value, self.volume),
            "XfyunMaxChunk" | "MaxChunk" => {
                self.max_chunk = value.parse::<usize>().unwrap_or(self.max_chunk).clamp(20, 1000)
            }
            "XfyunLogLevel" | "LogLevel" => self.log_level = parse_level(value).unwrap_or(self.log_level),
            "XfyunLogFile" | "LogFile" => {
                self.log_file = if value.is_empty() { None } else { Some(path(value)) }
            }
            "XfyunPlayer" | "Player" => {
                self.player = if value.is_empty() { None } else { Some(value.to_string()) }
            }
            "XfyunServerAudio" | "ServerAudio" => self.server_audio = truthy(value),
            "XfyunAuthCheckInterval" | "AuthCheckInterval" => {
                self.auth_check_interval = value.parse::<u32>().unwrap_or(self.auth_check_interval)
            }
            "XfyunReexec" | "Reexec" => self.reexec = truthy(value),
            // speech-dispatcher 通用项：Debug 1 打开模块调试日志
            "Debug" => {
                if truthy(value) && self.log_level < crate::log::DEBUG {
                    self.log_level = crate::log::DEBUG;
                }
            }
            _ => return false,
        }
        true
    }

    fn apply_env(&mut self) {
        for (name, slot) in [
            ("XFYUN_SDK_ROOT", 0usize),
            ("XFYUN_LIBS_DIR", 1),
            ("XFYUN_RES_DIR", 2),
            ("XFYUN_WORK_DIR", 3),
            ("XFYUN_LICENSE_FILE", 4),
            ("XFYUN_PLAYER", 5),
            ("XFYUN_LOG_FILE", 6),
        ] {
            if let Ok(v) = std::env::var(name) {
                if v.trim().is_empty() {
                    continue;
                }
                let v = v.trim().to_string();
                match slot {
                    0 => self.sdk_root = PathBuf::from(&v),
                    1 => self.libs_dir = PathBuf::from(&v),
                    2 => self.res_dir = PathBuf::from(&v),
                    3 => self.work_dir = PathBuf::from(&v),
                    4 => self.license_file = Some(PathBuf::from(&v)),
                    5 => self.player = Some(v),
                    _ => self.log_file = Some(PathBuf::from(&v)),
                }
            }
        }
        if let Ok(v) = std::env::var("XFYUN_OFFLINE_ONLY") {
            self.offline_only = truthy(&v);
        }
        if let Ok(v) = std::env::var("XFYUN_LOG_LEVEL") {
            if let Some(l) = parse_level(&v) {
                self.log_level = l;
            }
        }
    }

    /// 相对路径转绝对路径（模块的工作目录由 speech-dispatcher 决定，不能依赖 cwd）。
    pub fn normalize(&mut self) {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let absolutize = |p: &Path| -> PathBuf {
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                cwd.join(p)
            }
        };
        self.sdk_root = absolutize(&self.sdk_root);
        self.libs_dir = absolutize(&self.libs_dir);
        self.res_dir = absolutize(&self.res_dir);
        self.work_dir = absolutize(&self.work_dir);
        self.license_file = self.license_file.as_ref().map(|p| absolutize(p));
        self.log_file = self.log_file.as_ref().map(|p| absolutize(p));
    }

    /// SDK 资源是否齐备（xtts 资源缺失会直接导致 18105）。
    pub fn resources_ok(&self) -> bool {
        self.res_dir.join("xtts").is_dir() || self.work_dir.join("xtts").is_dir()
    }
}

fn strip_comment(line: &str) -> &str {
    match line.find('#') {
        Some(i) => &line[..i],
        None => line,
    }
}

fn unquote(v: &str) -> String {
    let v = v.trim();
    if v.len() >= 2 {
        let bytes = v.as_bytes();
        if (bytes[0] == b'"' && bytes[v.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[v.len() - 1] == b'\'')
        {
            return v[1..v.len() - 1].to_string();
        }
    }
    v.to_string()
}

fn truthy(v: &str) -> bool {
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "1" | "on" | "yes" | "true" | "enable" | "enabled"
    )
}

fn clamp_u8(v: &str, default: u8) -> u8 {
    v.trim().parse::<i32>().map(|n| n.clamp(0, 100) as u8).unwrap_or(default)
}

fn parse_level(v: &str) -> Option<u8> {
    match v.trim().to_ascii_lowercase().as_str() {
        "0" | "error" | "err" => Some(crate::log::ERROR),
        "1" | "warn" | "warning" => Some(crate::log::WARN),
        "2" | "info" => Some(crate::log::INFO),
        "3" | "debug" => Some(crate::log::DEBUG),
        "4" | "trace" | "verbose" => Some(crate::log::TRACE),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dotconf_style() {
        let mut c = Config::default();
        c.apply_conf(
            r#"
# 注释
Debug 1
XfyunWorkDir "/tmp/w"
XfyunDefaultVoice xiaofeng
XfyunSpeed 80
XfyunOfflineOnly 1
SomeUnknownThing "x y"
"#,
        );
        assert_eq!(c.work_dir, PathBuf::from("/tmp/w"));
        assert_eq!(c.default_voice, "xiaofeng");
        assert_eq!(c.speed, 80);
        assert!(c.offline_only);
        assert_eq!(c.log_level, crate::log::DEBUG);
        assert!(c.unknown.contains_key("SomeUnknownThing"));
    }

    #[test]
    fn speed_is_clamped_and_bad_values_keep_default() {
        let mut c = Config::default();
        c.set("XfyunSpeed", "999");
        assert_eq!(c.speed, 100);
        c.set("XfyunSpeed", "abc");
        assert_eq!(c.speed, 100);
        c.set("XfyunPitch", "-5");
        assert_eq!(c.pitch, 0);
    }

    #[test]
    fn relative_paths_become_absolute() {
        let mut c = Config::default();
        c.set("XfyunWorkDir", "rel/work");
        c.normalize();
        assert!(c.work_dir.is_absolute());
    }
}
